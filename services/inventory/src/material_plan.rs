use crate::{
    line_key,
    netting::{self, GrossPlan, Item, NetError, NetRow, Node},
    preview::{self, Warehouse},
    query::{Inventory, authorize},
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
};
use erp4us_identity::Scope;
use futures_util::FutureExt;
use http_body_util::{BodyExt, Full, Limited};
use hyper::Request;
use hyper_util::rt::TokioIo;
use mongodb::{
    ClientSession, Collection, Database,
    bson::{self, Bson, Document, doc},
    error::Error,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{net::TcpStream, task::JoinSet, time::timeout};
use uuid::Uuid;

pub(super) mod handoff;

const CONFLICT: StatusCode = StatusCode::CONFLICT;
const UNAVAILABLE: StatusCode = StatusCode::SERVICE_UNAVAILABLE;
const READ: &str = "inventory.material-plan.read";
const CREATE: &str = "inventory.material-plan.create";

#[derive(Deserialize, Serialize)]
pub(super) struct Plan {
    #[serde(with = "uuid::serde::hyphenated")]
    id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    order_id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    line_id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    warehouse_id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    source_event_id: Uuid,
    sku: String,
    product_revision: String,
    pieces: String,
    unit: String,
    created_at: String,
    production: Vec<PlanRow>,
    materials: Vec<PlanRow>,
}

#[derive(Deserialize, Serialize)]
struct PlanRow {
    #[serde(flatten)]
    item: Item,
    requested: String,
    reserved: String,
    uncovered: String,
}

impl PlanRow {
    fn new(item: Item, row: NetRow) -> Self {
        Self {
            item,
            requested: row.requested.to_string(),
            reserved: row.reserved.to_string(),
            uncovered: row.uncovered().to_string(),
        }
    }
}

#[derive(Deserialize)]
struct Record {
    plan: Plan,
}

struct Demand {
    order: Uuid,
    line: Uuid,
    warehouse: Warehouse,
    root: Item,
    pieces: i32,
    event: Uuid,
    source_stock: Document,
}

impl Demand {
    fn read(scope: Scope, order: Uuid, line: Uuid, row: &Document) -> Option<Self> {
        let stock = row.get_document("stock").ok()?;
        if stock.get_str("unit") != Ok("EA")
            || stock.get_str("tenantId").ok()? != scope.tenant.to_string()
            || stock.get_str("organizationId").ok()? != scope.organization.to_string()
        {
            return None;
        }
        let pieces = row.get_i32("outstanding").ok()?;
        if !(0..=1000).contains(&pieces) {
            return None;
        }
        Some(Self {
            order,
            line,
            pieces,
            warehouse: Warehouse {
                tenant: scope.tenant,
                organization: scope.organization,
                id: Uuid::parse_str(stock.get_str("warehouseId").ok()?).ok()?,
            },
            root: Item {
                sku: stock.get_str("catalogSku").ok()?.into(),
                revision: Some(stock.get_str("productRevision").ok()?.into()),
            },
            event: Uuid::parse_str(row.get_str("sourceEventId").ok()?).ok()?,
            source_stock: stock.clone(),
        })
    }
}

#[derive(Debug)]
enum PlanningFailure {
    LineChanged,
    Net(NetError),
}

fn key(scope: Scope, order: Uuid, line: Uuid) -> Result<Document, StatusCode> {
    if order.is_nil() || line.is_nil() {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(line_key(
        &preview::scoped(scope.tenant, scope.organization),
        order,
        line,
    ))
}

fn plans(db: &Database) -> Collection<Record> {
    db.collection("material_plans")
}

pub(super) async fn get(
    State(state): State<Inventory>,
    headers: HeaderMap,
    Path((order, line)): Path<(Uuid, Uuid)>,
) -> Result<Json<Plan>, StatusCode> {
    let scope = authorize(&state.auth, &headers, READ)?;
    let id = key(scope, order, line)?;
    plans(&state.db)
        .find_one(doc! {"_id": id})
        .await
        .map_err(|_| UNAVAILABLE)?
        .map(|record| Json(record.plan))
        .ok_or(StatusCode::NOT_FOUND)
}

pub(super) async fn create(
    State(state): State<Inventory>,
    headers: HeaderMap,
    Path((order, line)): Path<(Uuid, Uuid)>,
    body: Bytes,
) -> Result<(StatusCode, Json<Plan>), StatusCode> {
    let scope = authorize(&state.auth, &headers, CREATE)?;
    authorize(&state.auth, &headers, "manufacturing.plan.read")?;
    if !body.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let id = key(scope, order, line)?;
    if let Some(record) = plans(&state.db)
        .find_one(doc! {"_id": &id})
        .await
        .map_err(|_| UNAVAILABLE)?
    {
        return Ok((StatusCode::OK, Json(record.plan)));
    }
    let row = preview::reservations(&state.db)
        .find_one(doc! {"_id": &id, "sourceEventId": {"$exists": true}})
        .await
        .map_err(|_| UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let demand = Demand::read(scope, order, line, &row).ok_or(UNAVAILABLE)?;
    if demand.pieces == 0 || row.contains_key("cancellation") {
        return Err(CONFLICT);
    }
    let tree = fetch_pinned(&headers, scope, &demand).await?;
    let (plan, created) = persist(&state.db, scope, id, &demand, &tree)
        .await
        .map_err(|error| match error.get_custom::<PlanningFailure>() {
            Some(
                PlanningFailure::LineChanged
                | PlanningFailure::Net(NetError::InvalidStructure | NetError::QuantityLimit),
            ) => CONFLICT,
            _ => UNAVAILABLE,
        })?;
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(plan),
    ))
}

async fn fetch_pinned(
    headers: &HeaderMap,
    scope: Scope,
    demand: &Demand,
) -> Result<Vec<Node>, StatusCode> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let document = serde_json::to_vec(&serde_json::json!({
        "sku": &demand.root.sku, "revision": &demand.root.revision, "pieces": demand.pieces.to_string(),
    })).map_err(|_| UNAVAILABLE)?;
    let request = Request::post("/v1/manufacturing/requirements:preview")
        .header(header::HOST, "manufacturing")
        .header(header::CONNECTION, "close")
        .header(header::AUTHORIZATION, bearer)
        .header(header::CONTENT_TYPE, "application/json")
        .header(erp4us_identity::TENANT_HEADER, scope.tenant.to_string())
        .header(
            erp4us_identity::ORGANIZATION_HEADER,
            scope.organization.to_string(),
        )
        .body(Full::new(Bytes::from(document)))
        .map_err(|_| UNAVAILABLE)?;
    timeout(Duration::from_secs(3), async move {
        // Fixed Compose peer and path; there is no browser-provided destination or redirect.
        let socket = TcpStream::connect(("manufacturing", 8090))
            .await
            .map_err(|_| UNAVAILABLE)?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(socket))
            .await
            .map_err(|_| UNAVAILABLE)?;
        let mut task = JoinSet::new();
        task.spawn(connection);
        let response = sender
            .send_request(request)
            .await
            .map_err(|_| UNAVAILABLE)?;
        match response.status() {
            StatusCode::OK => {
                let body = Limited::new(response.into_body(), 262_144)
                    .collect()
                    .await
                    .map_err(|_| UNAVAILABLE)?;
                let plan: GrossPlan =
                    serde_json::from_slice(&body.to_bytes()).map_err(|_| UNAVAILABLE)?;
                plan.checked(&demand.root, demand.pieces)
                    .map_err(|_| CONFLICT)
            }
            StatusCode::NOT_FOUND | StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY => {
                Err(CONFLICT)
            }
            status @ (StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED) => Err(status),
            _ => Err(UNAVAILABLE),
        }
    })
    .await
    .map_err(|_| UNAVAILABLE)?
}

async fn persist(
    db: &Database,
    scope: Scope,
    id: Document,
    demand: &Demand,
    tree: &[Node],
) -> mongodb::error::Result<(Plan, bool)> {
    let plan_id = Uuid::new_v4();
    let created_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(Error::custom)?;
    let stock_keys: BTreeMap<_, _> = netting::items(tree)
        .into_iter()
        .map(|item| {
            let key = demand
                .warehouse
                .stock_key(&item.sku, item.revision.as_deref());
            (item, key)
        })
        .collect();
    let selectors = stock_keys.values().cloned().collect::<Vec<_>>();
    let ctx = PlanTransaction {
        plans: plans(db),
        reservations: preview::reservations(db),
        balances: db.collection("balances"),
        id,
        demand,
        tree,
        stock_keys,
        selectors,
        plan_id,
        created_at,
        actor: scope.actor,
    };
    let mut session = db.client().start_session().await?;
    session
        .start_transaction()
        .and_run(ctx, |session, ctx| commit_plan(session, ctx).boxed())
        .await
}

struct PlanTransaction<'a> {
    plans: Collection<Record>,
    reservations: Collection<Document>,
    balances: Collection<Document>,
    id: Document,
    demand: &'a Demand,
    tree: &'a [Node],
    stock_keys: BTreeMap<Item, Document>,
    selectors: Vec<Document>,
    plan_id: Uuid,
    created_at: String,
    actor: Uuid,
}

async fn commit_plan(
    session: &mut ClientSession,
    ctx: &PlanTransaction<'_>,
) -> mongodb::error::Result<(Plan, bool)> {
    let PlanTransaction {
        plans,
        reservations,
        balances,
        id,
        demand,
        tree,
        stock_keys,
        selectors,
        plan_id,
        created_at,
        actor,
    } = ctx;
    if let Some(existing) = plans
        .find_one(doc! {"_id": id})
        .session(&mut *session)
        .await?
    {
        return Ok((existing.plan, false));
    }
    // The input must still be the exact open reservation used to resolve
    // the pinned BOM. Writing the same row serializes competing commands.
    let claimed = reservations
        .update_one(
            doc! {
                "_id": id, "sourceEventId": demand.event.to_string(), "stock": &demand.source_stock,
                "outstanding": demand.pieces, "materialPlanId": {"$exists": false},
                "cancellation": {"$exists": false}
            },
            doc! {"$set": {"materialPlanId": plan_id.to_string()}},
        )
        .session(&mut *session)
        .await?;
    if claimed.matched_count != 1 {
        return Err(Error::custom(PlanningFailure::LineChanged));
    }
    let mut available = BTreeMap::new();
    let mut cursor = balances
        .find(doc! {"_id": {"$in": selectors}})
        .session(&mut *session)
        .await?;
    while let Some(row) = cursor.next(&mut *session).await {
        let row = row?;
        let key = row.get_document("_id").map_err(Error::custom)?;
        let revision = match key.get("productRevision") {
            Some(Bson::Null) => None,
            Some(Bson::String(value)) => Some(value.clone()),
            _ => return Err(Error::custom(PlanningFailure::LineChanged)),
        };
        let item = Item {
            sku: key.get_str("catalogSku").map_err(Error::custom)?.into(),
            revision,
        };
        available.insert(item, row.get_i32("available").map_err(Error::custom)?);
    }
    let rows = netting::allocate(tree, demand.pieces, available)
        .map_err(|error| Error::custom(PlanningFailure::Net(error)))?;
    let mut production = vec![PlanRow::new(
        demand.root.clone(),
        NetRow {
            requested: demand.pieces,
            reserved: 0,
        },
    )];
    let mut materials = Vec::new();
    for (item, allocation) in rows {
        if allocation.reserved > 0 {
            let stock = stock_keys
                .get(&item)
                .ok_or_else(|| Error::custom(PlanningFailure::LineChanged))?;
            let updated = balances
                .update_one(
                    doc! {"_id": stock, "available": {"$gte": allocation.reserved}},
                    doc! {"$inc": {"available": -allocation.reserved}},
                )
                .session(&mut *session)
                .await?;
            if updated.matched_count != 1 {
                return Err(Error::custom(PlanningFailure::LineChanged));
            }
        }
        let is_assembly = item.revision.is_some();
        let row = PlanRow::new(item, allocation);
        if is_assembly {
            production.push(row);
        } else {
            materials.push(row);
        }
    }
    let plan = Plan {
        id: *plan_id,
        order_id: demand.order,
        line_id: demand.line,
        warehouse_id: demand.warehouse.id,
        source_event_id: demand.event,
        sku: demand.root.sku.clone(),
        product_revision: demand
            .root
            .revision
            .clone()
            .ok_or_else(|| Error::custom(PlanningFailure::LineChanged))?,
        pieces: demand.pieces.to_string(),
        unit: "EA".into(),
        created_at: created_at.clone(),
        production,
        materials,
    };
    plans
        .clone_with_type::<Document>()
        .insert_one(doc! {
            "_id": id, "schemaVersion": 1, "createdBy": actor.to_string(),
            "structure": bson::to_bson(tree).map_err(Error::custom)?,
            "plan": bson::to_document(&plan).map_err(Error::custom)?,
        })
        .session(session)
        .await?;
    Ok((plan, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_plan_reads_through_the_mongodb_wire_decoder() {
        let id = "ea0c1030-10d8-42f4-8bbc-f42687ef52ec";
        let view = serde_json::json!({
            "id": id, "order_id": id, "line_id": id, "warehouse_id": id, "source_event_id": id,
            "sku": "NM-1000", "product_revision": "A", "pieces": "10", "unit": "EA",
            "created_at": "2026-11-28T08:00:00Z",
            "production": [{"sku": "NM-1000", "revision": "A", "requested": "10", "reserved": "0", "uncovered": "10"}],
            "materials": [{"sku": "NM-ENDPLATE", "revision": null, "requested": "20", "reserved": "8", "uncovered": "12"}],
        });
        let plan: Plan = serde_json::from_value(view.clone()).unwrap();
        let record = doc! { "plan": bson::to_document(&plan).unwrap() };
        let wire = bson::to_vec(&record).unwrap();
        let decoded: Record = bson::from_slice(&wire).unwrap();
        assert_eq!(serde_json::to_value(decoded.plan).unwrap(), view);
    }
}
