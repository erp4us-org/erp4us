use crate::{
    delivery::{self, ConfirmedOrder},
    domain::{CreateDraft, Draft},
};
use erp4us_identity::Scope;
use erp4us_messaging::Envelope;
use mongodb::{
    Collection, Database, IndexModel,
    bson::{self, DateTime, Document, doc, oid::ObjectId},
    error::{Error, ErrorKind, WriteFailure},
    options::{IndexOptions, ReturnDocument},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Deserialize, Serialize)]
pub(crate) struct SavedOrder {
    schema_version: u32,
    tenant_id: String,
    organization_id: String,
    created_by: String,
    input: CreateDraft,
    pub draft: Draft,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    confirmation: Option<Confirmation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfirmOrder {
    pub command_id: Uuid,
    pub expected_revision: u32,
}

#[derive(Deserialize, Serialize)]
struct Confirmation {
    #[serde(with = "uuid::serde::hyphenated")]
    command_id: Uuid,
    expected_revision: u32,
    confirmed_by: String,
    confirmed_at: String,
    #[serde(with = "uuid::serde::hyphenated")]
    message_id: Uuid,
    envelope_json: String,
    pending: bool,
}

#[derive(Deserialize)]
pub(crate) struct PendingEvent {
    #[serde(rename = "_id")]
    storage_id: ObjectId,
    #[serde(with = "uuid::serde::hyphenated")]
    tenant_id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    organization_id: Uuid,
    draft: OrderId,
    confirmation: Confirmation,
}

#[derive(Deserialize)]
struct OrderId {
    #[serde(with = "uuid::serde::hyphenated")]
    id: Uuid,
}

impl PendingEvent {
    pub(crate) fn envelope(&self) -> Result<Envelope<ConfirmedOrder>, erp4us_messaging::Error> {
        use erp4us_messaging::Error;
        let event: Envelope<ConfirmedOrder> =
            serde_json::from_str(&self.confirmation.envelope_json).map_err(Error::Deserialize)?;
        if event.tenant_id != Some(self.tenant_id)
            || event.organization_id != Some(self.organization_id)
            || event.message_id != self.confirmation.message_id
            || event.payload.order_id != self.draft.id
        {
            return Err(Error::InvalidEnvelope("sales outbox identity"));
        }
        event.validate()?;
        Ok(event)
    }
}

impl SavedOrder {
    pub(crate) fn new(scope: Scope, input: CreateDraft, draft: Draft) -> Self {
        Self {
            schema_version: 1,
            tenant_id: scope.tenant.to_string(),
            organization_id: scope.organization.to_string(),
            created_by: scope.actor.to_string(),
            input,
            draft,
            confirmation: None,
        }
    }

    pub(crate) fn is_retry(&self, scope: Scope, input: &CreateDraft) -> bool {
        self.created_by == scope.actor.to_string() && &self.input == input
    }

    pub(crate) fn is_same_command_as(&self, other: &Self) -> bool {
        self.created_by == other.created_by && self.input == other.input
    }

    pub(crate) fn view(self) -> Draft {
        if let Some(confirmation) = self.confirmation {
            self.draft
                .confirmed(confirmation.confirmed_at, confirmation.message_id)
        } else {
            self.draft
        }
    }

    fn confirm_retry(self, scope: Scope, input: &ConfirmOrder) -> Confirm {
        if self.confirmation.as_ref().is_some_and(|saved| {
            saved.command_id == input.command_id
                && saved.expected_revision == input.expected_revision
                && saved.confirmed_by == scope.actor.to_string()
        }) {
            Confirm::Saved(Box::new(self.view()))
        } else {
            Confirm::Conflict
        }
    }
}

#[derive(Clone)]
pub(crate) struct Store {
    db: Database,
    orders: Collection<SavedOrder>,
}

pub(crate) enum Insert {
    Created,
    AlreadyExists,
}

pub(crate) enum Confirm {
    Saved(Box<Draft>),
    Missing,
    Conflict,
}

fn scoped(scope: Scope) -> Document {
    doc! { "tenant_id": scope.tenant.to_string(), "organization_id": scope.organization.to_string() }
}

impl Store {
    pub(crate) async fn open(db: Database) -> Result<Self, Error> {
        let orders = db.collection("orders");
        orders
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "tenant_id": 1, "organization_id": 1, "draft.id": 1 })
                    .options(IndexOptions::builder().unique(true).build())
                    .build(),
            )
            .await?;
        orders
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "tenant_id": 1, "confirmation.pending": 1, "_id": 1 })
                    .options(
                        IndexOptions::builder()
                            .partial_filter_expression(doc! {"confirmation.pending": true})
                            .build(),
                    )
                    .build(),
            )
            .await?;
        Ok(Self { db, orders })
    }

    pub(crate) async fn ready(&self) -> bool {
        self.db.run_command(doc! {"ping": 1}).await.is_ok()
    }

    pub(crate) async fn insert(&self, order: &SavedOrder) -> Result<Insert, Error> {
        match self.orders.insert_one(order).await {
            Ok(_) => Ok(Insert::Created),
            Err(error) => match error.kind.as_ref() {
                ErrorKind::Write(WriteFailure::WriteError(error)) if error.code == 11000 => {
                    Ok(Insert::AlreadyExists)
                }
                _ => Err(error),
            },
        }
    }

    pub(crate) async fn get(&self, scope: Scope, id: Uuid) -> Result<Option<SavedOrder>, Error> {
        let mut filter = scoped(scope);
        filter.insert("draft.id", id.to_string());
        self.orders.find_one(filter).await
    }

    pub(crate) async fn page(
        &self,
        scope: Scope,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<(Vec<Draft>, Option<Uuid>), Error> {
        let mut filter = scoped(scope);
        if let Some(after) = after {
            filter.insert("draft.id", doc! { "$gt": after.to_string() });
        }
        let mut items: Vec<Draft> = Vec::with_capacity(limit);
        let mut cursor = self
            .orders
            .find(filter)
            .sort(doc! {"draft.id": 1})
            .limit(limit as i64 + 1)
            .await?;
        while cursor.advance().await? {
            if items.len() == limit {
                let next = items.last().map(|draft| draft.id);
                return Ok((items, next));
            }
            items.push(cursor.deserialize_current()?.view());
        }
        Ok((items, None))
    }

    pub(crate) async fn confirm(
        &self,
        scope: Scope,
        id: Uuid,
        input: ConfirmOrder,
        traceparent: Option<String>,
    ) -> Result<Confirm, Error> {
        let Some(order) = self.get(scope, id).await? else {
            return Ok(Confirm::Missing);
        };
        if order.confirmation.is_some() || input.expected_revision != 1 {
            return Ok(order.confirm_retry(scope, &input));
        }
        let event = delivery::event(scope, order.draft.confirmation_payload(), traceparent)
            .map_err(Error::custom)?;
        let confirmation = Confirmation {
            command_id: input.command_id,
            expected_revision: input.expected_revision,
            confirmed_by: scope.actor.to_string(),
            confirmed_at: event.timestamp.clone(),
            message_id: event.message_id,
            envelope_json: serde_json::to_string(&event).map_err(Error::custom)?,
            pending: true,
        };
        let mut filter = scoped(scope);
        filter.extend(doc! {
            "draft.id": id.to_string(),
            "draft.revision": i64::from(input.expected_revision),
            "confirmation": { "$exists": false },
        });
        // The immutable snapshot, confirmation and pending event live in one
        // document; this compare-and-set commits the state and the outbox together.
        if let Some(saved) = self.orders
            .find_one_and_update(
                filter,
                doc! { "$set": {"confirmation": bson::to_document(&confirmation).map_err(Error::custom)? } },
            )
            .return_document(ReturnDocument::After)
            .await?
        {
            return Ok(Confirm::Saved(Box::new(saved.view())));
        }
        Ok(match self.get(scope, id).await? {
            Some(saved) => saved.confirm_retry(scope, &input),
            None => Confirm::Missing,
        })
    }

    pub(crate) async fn next_event(&self, tenant: Uuid) -> Result<Option<PendingEvent>, Error> {
        self.orders
            .clone_with_type()
            .find_one(doc! {
                "tenant_id": tenant.to_string(), "confirmation.pending": true
            })
            .projection(
                doc! { "draft.id": 1, "tenant_id": 1, "organization_id": 1, "confirmation": 1 },
            )
            .sort(doc! {"_id": 1})
            .await
    }

    pub(crate) async fn mark_sent(&self, pending: &PendingEvent) -> Result<(), Error> {
        self.orders.update_one(
            doc! {
                "_id": pending.storage_id,
                "tenant_id": pending.tenant_id.to_string(),
                "organization_id": pending.organization_id.to_string(),
                "confirmation.message_id": pending.confirmation.message_id.to_string(),
                "confirmation.pending": true,
            },
            doc! {"$set": {"confirmation.pending": false, "confirmation.published_at": DateTime::now()}},
        ).await?;
        Ok(())
    }
}
