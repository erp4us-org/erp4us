use crate::{
    domain::Scope,
    http::{problem, unavailable},
    store::Store,
};
use axum::{
    Extension, Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use mongodb::{
    Collection, Database, IndexModel,
    bson::{self, doc},
    error::{ErrorKind, WriteFailure},
    options::IndexOptions,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    Customer,
    Supplier,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegisterPartner {
    code: String,
    name: String,
    roles: Vec<Role>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<Profile>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    contact: String,
    address: Address,
    delivery_address: Address,
    payment_days: u16,
    currency: String,
    language: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Address {
    street: String,
    postal_code: String,
    city: String,
    country: String,
}

fn text(value: &str) -> bool {
    (1..=120).contains(&value.chars().count())
        && !value.trim().is_empty()
        && !value.chars().any(char::is_control)
}

impl Address {
    fn is_valid(&self) -> bool {
        text(&self.street)
            && text(&self.postal_code)
            && text(&self.city)
            && self.country.len() == 2
            && self.country.bytes().all(|c| c.is_ascii_uppercase())
    }
}

impl Profile {
    fn is_valid(&self) -> bool {
        text(&self.contact)
            && self.address.is_valid()
            && self.delivery_address.is_valid()
            && self.payment_days <= 365
            && self.currency.len() == 3
            && self.currency.bytes().all(|c| c.is_ascii_uppercase())
            && self.language.len() == 2
            && self.language.bytes().all(|c| c.is_ascii_lowercase())
    }
}

#[derive(Deserialize, Serialize)]
struct Partner {
    #[serde(rename = "_id", with = "uuid::serde::hyphenated")]
    id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    tenant_id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    organization_id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    created_by: Uuid,
    schema_version: u32,
    revision: u32,
    #[serde(flatten)]
    details: RegisterPartner,
}

#[derive(Serialize)]
struct PartnerView {
    id: Uuid,
    tenant_id: Uuid,
    organization_id: Uuid,
    created_by: Uuid,
    schema_version: u32,
    revision: u32,
    #[serde(flatten)]
    details: RegisterPartner,
}

impl Partner {
    fn new(scope: Scope, details: RegisterPartner) -> Result<Self, &'static str> {
        if !(1..=32).contains(&details.code.len())
            || !details
                .code
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err("Partner code must have 1-32 uppercase ASCII letters, digits or hyphens");
        }
        if !text(&details.name) {
            return Err(
                "Partner name must have 1-120 characters, without controls, and cannot be blank",
            );
        }
        match details.roles.as_slice() {
            [_] => {}
            [first, second] if first != second => {}
            _ => return Err("Choose customer, supplier, or both without duplicates"),
        }
        if details
            .profile
            .as_ref()
            .is_some_and(|profile| !profile.is_valid())
        {
            return Err("Invalid partner contact, address, payment terms, currency or language");
        }
        Ok(Self {
            id: Uuid::new_v4(),
            tenant_id: scope.tenant,
            organization_id: scope.organization,
            created_by: scope.actor,
            revision: 1,
            schema_version: 1,
            details,
        })
    }

    fn view(self) -> PartnerView {
        PartnerView {
            id: self.id,
            tenant_id: self.tenant_id,
            organization_id: self.organization_id,
            created_by: self.created_by,
            schema_version: self.schema_version,
            revision: self.revision,
            details: self.details,
        }
    }
}

fn collection(database: &Database) -> Collection<Partner> {
    database.collection("partners")
}

pub(crate) async fn seed_preview(scope: Scope, store: &Store) -> std::io::Result<()> {
    let details: Vec<RegisterPartner> =
        serde_json::from_str(include_str!("novamotion-partners.json"))
            .map_err(std::io::Error::other)?;
    let partners = details
        .into_iter()
        .map(|details| Partner::new(scope, details))
        .collect::<Result<Vec<_>, _>>()
        .map_err(std::io::Error::other)?;
    let target = collection(store.database());
    for partner in partners {
        let payload = bson::to_document(&partner).map_err(std::io::Error::other)?;
        target
            .update_one(
                doc! {
                    "tenant_id": scope.tenant.to_string(),
                    "organization_id": scope.organization.to_string(),
                    "code": &partner.details.code,
                },
                doc! { "$setOnInsert": payload },
            )
            .upsert(true)
            .await
            .map_err(|_| std::io::Error::other("Partner seed unavailable"))?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PartnersQuery {
    role: Option<Role>,
    after: Option<String>,
    limit: Option<u8>,
}

#[derive(Serialize)]
struct PartnersPage {
    items: Vec<PartnerView>,
    next_after: Option<String>,
}

pub(crate) async fn list(
    State(store): State<Store>,
    Extension(scope): Extension<Scope>,
    Query(query): Query<PartnersQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(25);
    if !(1..=50).contains(&limit) {
        return problem(StatusCode::BAD_REQUEST, "Limit must be between 1 and 50");
    }
    let mut filter = doc! {
        "tenant_id": scope.tenant.to_string(),
        "organization_id": scope.organization.to_string(),
    };
    if let Some(role) = query.role {
        filter.insert(
            "roles",
            match role {
                Role::Customer => "customer",
                Role::Supplier => "supplier",
            },
        );
    }
    if let Some(after) = query.after {
        if after.is_empty()
            || after.len() > 32
            || !after
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
        {
            return problem(StatusCode::BAD_REQUEST, "Invalid partner page cursor");
        }
        filter.insert("code", doc! {"$gt": after});
    }
    let limit = usize::from(limit);
    let result = async {
        let mut cursor = collection(store.database())
            .find(filter)
            .sort(doc! {"code":1})
            .limit(limit as i64 + 1)
            .await?;
        let mut items = Vec::with_capacity(limit);
        let mut next_after = None;
        while cursor.advance().await? {
            if items.len() == limit {
                next_after = items
                    .last()
                    .map(|item: &PartnerView| item.details.code.clone());
                break;
            }
            items.push(cursor.deserialize_current()?.view());
        }
        Ok::<_, mongodb::error::Error>(PartnersPage { items, next_after })
    }
    .await;
    match result {
        Ok(page) => Json(page).into_response(),
        Err(error) => unavailable("list_partners", error),
    }
}

pub(crate) async fn ensure_index(database: &Database) -> mongodb::error::Result<()> {
    collection(database)
        .create_index(
            IndexModel::builder()
                .keys(doc! {"tenant_id": 1, "organization_id": 1, "code": 1})
                .options(
                    IndexOptions::builder()
                        .name("partner_scope_code_v1".to_owned())
                        .unique(true)
                        .build(),
                )
                .build(),
        )
        .await?;
    Ok(())
}

pub(crate) async fn register(
    State(store): State<Store>,
    Extension(scope): Extension<Scope>,
    body: Result<Json<RegisterPartner>, JsonRejection>,
) -> Response {
    let Json(command) = match body {
        Ok(body) => body,
        Err(rejection) => return problem(rejection.status(), "Invalid JSON request"),
    };
    let partner = match Partner::new(scope, command) {
        Ok(partner) => partner,
        Err(detail) => return problem(StatusCode::UNPROCESSABLE_ENTITY, detail),
    };
    match collection(store.database()).insert_one(&partner).await {
        Ok(_) => (
            StatusCode::CREATED,
            [(header::LOCATION, format!("/v1/partners/{}", partner.id))],
            Json(partner.view()),
        )
            .into_response(),
        Err(error) => match error.kind.as_ref() {
            ErrorKind::Write(WriteFailure::WriteError(e)) if e.code == 11000 => {
                problem(StatusCode::CONFLICT, "Partner code already registered")
            }
            _ => unavailable("register_partner", error),
        },
    }
}

pub(crate) async fn find(
    State(store): State<Store>,
    Extension(scope): Extension<Scope>,
    Path(id): Path<String>,
) -> Response {
    let Ok(id) = Uuid::parse_str(&id) else {
        return problem(StatusCode::BAD_REQUEST, "Invalid partner ID");
    };
    match collection(store.database())
        .find_one(doc! {
            "_id": id.to_string(),
            "tenant_id": scope.tenant.to_string(),
            "organization_id": scope.organization.to_string(),
        })
        .await
    {
        Ok(Some(partner)) => Json(partner.view()).into_response(),
        Ok(None) => problem(StatusCode::NOT_FOUND, "Partner not found"),
        Err(error) => unavailable("find_partner", error),
    }
}
