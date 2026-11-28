use super::{CONFLICT, Plan, READ, UNAVAILABLE, key};
use crate::{
    netting::{Item, positive},
    preview::{self, Warehouse},
    query::{Inventory, authorize},
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use erp4us_identity::Scope as IdentityScope;
use erp4us_messaging::{
    Actor, ConnectOptions, Connection, Contract, Delivery, Envelope, Error, Kind, Layer, Route,
    Scope, Version,
};
use mongodb::{
    Collection, Database, IndexModel,
    bson::{self, DateTime, Document, doc},
    options::{IndexOptions, ReturnDocument},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const SEND: &str = "inventory.material-plan.send";
const STREAM: &str = "ERP4US_BUSINESS_EVENTS_V1";
const SOURCE: &str = "erp4us.inventory";
const DEADLINE: Duration = Duration::from_secs(5);
const MAX_ROWS: usize = 32;
// Leave room for the shared envelope headers inside the broker's 8 KiB HPUB limit.
const MAX_PAYLOAD: usize = 7 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct NettedPlan {
    plan_id: Uuid,
    order_id: Uuid,
    line_id: Uuid,
    bom: Bom,
    buy_rows: Vec<BuyRow>,
}

#[derive(Deserialize, Serialize)]
struct Bom {
    sku: String,
    revision: String,
}

#[derive(Deserialize, Serialize)]
struct BuyRow {
    #[serde(flatten)]
    item: Item,
    unit: String,
    pieces: String,
}

impl Contract for NettedPlan {
    const ROUTE: Route<'static> = Route {
        layer: Layer::Business,
        domain: "inventory",
        kind: Kind::Event,
        resource: "material-plan",
        action: "netted",
    };
    const SCHEMA_VERSION: &'static str = "1.0.0";
    const PROTOCOL_SUPPORT: &'static str = "=1.0.0";
    const SCHEMA_SUPPORT: &'static str = "=1.0.0";
    const DELIVERY: Delivery = Delivery::JetStream(STREAM);

    fn validate(&self) -> Result<(), Error> {
        let root = Item {
            sku: self.bom.sku.clone(),
            revision: Some(self.bom.revision.clone()),
        };
        let mut seen = BTreeSet::new();
        if self.plan_id.is_nil()
            || self.line_id.is_nil()
            || self.order_id.is_nil()
            || !root.valid()
            || !(1..=MAX_ROWS).contains(&self.buy_rows.len())
            || self.buy_rows.iter().any(|row| {
                !row.item.valid()
                    || row.unit != "EA"
                    || positive(&row.pieces).is_none()
                    || !seen.insert(&row.item)
            })
        {
            return Err(Error::InvalidEnvelope("material plan shortage"));
        }
        Ok(())
    }
}

impl NettedPlan {
    fn project(plan: &Plan) -> Self {
        let mut buy_rows: Vec<_> = plan
            .materials
            .iter()
            .filter(|row| row.uncovered != "0")
            .map(|row| BuyRow {
                item: row.item.clone(),
                unit: plan.unit.clone(),
                pieces: row.uncovered.clone(),
            })
            .collect();
        buy_rows.sort_by(|left, right| left.item.cmp(&right.item));
        Self {
            plan_id: plan.id,
            order_id: plan.order_id,
            line_id: plan.line_id,
            bom: Bom {
                sku: plan.sku.clone(),
                revision: plan.product_revision.clone(),
            },
            buy_rows,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct Handoff {
    #[serde(with = "uuid::serde::hyphenated")]
    event_id: Uuid,
    pending: bool,
    envelope: String,
}

#[derive(Deserialize)]
struct PreparedPlan {
    plan: Plan,
    handoff: Option<Handoff>,
}

#[derive(Serialize)]
pub(crate) struct View {
    plan_id: Uuid,
    event_id: Uuid,
    state: &'static str,
}

fn view(plan_id: Uuid, handoff: Handoff) -> View {
    View {
        plan_id,
        event_id: handoff.event_id,
        state: if handoff.pending {
            "pending"
        } else {
            "published"
        },
    }
}

fn records(db: &Database) -> Collection<PreparedPlan> {
    db.collection("material_plans")
}

pub(crate) async fn get(
    State(state): State<Inventory>,
    headers: HeaderMap,
    Path((order, line)): Path<(Uuid, Uuid)>,
) -> Result<Json<View>, StatusCode> {
    let scope = authorize(&state.auth, &headers, READ)?;
    let id = key(scope, order, line)?;
    let (plan, handoff) = records(&state.db)
        .find_one(doc! {"_id": id})
        .await
        .map_err(|_| UNAVAILABLE)?
        .and_then(|record| record.handoff.map(|handoff| (record.plan.id, handoff)))
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(view(plan, handoff)))
}

pub(crate) async fn send(
    State(state): State<Inventory>,
    headers: HeaderMap,
    Path((order, line)): Path<(Uuid, Uuid)>,
    body: Bytes,
) -> Result<(StatusCode, Json<View>), StatusCode> {
    let scope = authorize(&state.auth, &headers, SEND)?;
    if !body.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let id = key(scope, order, line)?;
    let records = records(&state.db);
    let record = records
        .find_one(doc! {"_id": &id})
        .await
        .map_err(|_| UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if let Some(handoff) = record.handoff {
        return Ok((StatusCode::OK, Json(view(record.plan.id, handoff))));
    }
    let payload = NettedPlan::project(&record.plan);
    payload.validate().map_err(|_| CONFLICT)?;
    let correlation = sales_correlation(&state.db, scope, record.plan.source_event_id).await?;
    let event = envelope(scope, payload, record.plan.source_event_id, correlation)?;
    let json = serde_json::to_string(&event).map_err(|_| UNAVAILABLE)?;
    if json.len() > MAX_PAYLOAD {
        return Err(CONFLICT);
    }
    let handoff = Handoff {
        event_id: event.message_id,
        pending: true,
        envelope: json,
    };
    let set = bson::to_document(&handoff).map_err(|_| UNAVAILABLE)?;
    if let Some(saved) = records
        .find_one_and_update(
            doc! {"_id": &id, "handoff": {"$exists": false}},
            doc! {"$set": {"handoff": set}},
        )
        .return_document(ReturnDocument::After)
        .await
        .map_err(|_| UNAVAILABLE)?
    {
        return Ok((
            StatusCode::CREATED,
            Json(view(saved.plan.id, saved.handoff.ok_or(UNAVAILABLE)?)),
        ));
    }
    let saved = records
        .find_one(doc! {"_id": id})
        .await
        .map_err(|_| UNAVAILABLE)?
        .ok_or(UNAVAILABLE)?;
    Ok((
        StatusCode::OK,
        Json(view(saved.plan.id, saved.handoff.ok_or(UNAVAILABLE)?)),
    ))
}

async fn sales_correlation(
    db: &Database,
    scope: IdentityScope,
    source: Uuid,
) -> Result<Uuid, StatusCode> {
    let mut key = preview::scoped(scope.tenant, scope.organization);
    key.insert("messageId", source.to_string());
    let row = db
        .collection::<Document>("sales_inbox")
        .find_one(doc! {"_id": key})
        .await
        .map_err(|_| UNAVAILABLE)?
        .ok_or(UNAVAILABLE)?;
    Uuid::try_parse(row.get_str("correlationId").map_err(|_| UNAVAILABLE)?).map_err(|_| UNAVAILABLE)
}

fn envelope(
    scope: IdentityScope,
    payload: NettedPlan,
    causation: Uuid,
    correlation: Uuid,
) -> Result<Envelope<NettedPlan>, StatusCode> {
    let event = Envelope {
        message_id: Uuid::new_v4(),
        message_type: NettedPlan::ROUTE
            .subject(1, Scope::Tenant(scope.tenant), 1)
            .map_err(|_| UNAVAILABLE)?,
        protocol_version: Version::new(1, 0, 0),
        schema_version: Version::new(1, 0, 0),
        service_version: Version::parse(env!("CARGO_PKG_VERSION")).map_err(|_| UNAVAILABLE)?,
        timestamp: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|_| UNAVAILABLE)?,
        source_service: SOURCE.into(),
        tenant_id: Some(scope.tenant),
        organization_id: Some(scope.organization),
        actor: Some(Actor {
            id: scope.actor.to_string(),
            kind: "user".into(),
        }),
        correlation_id: correlation,
        causation_id: Some(causation),
        traceparent: None,
        tracestate: None,
        payload,
    };
    event.validate().map_err(|_| UNAVAILABLE)?;
    Ok(event)
}

pub(crate) async fn relay() -> io::Result<()> {
    let scope = Warehouse::load()?;
    let db = preview::open().await?;
    let broker = Connection::connect(
        &preview::required("NATS_URL")?,
        ConnectOptions::with_user_and_password(
            "inventory_plan_relay".into(),
            preview::required("NATS_PASSWORD")?,
        )
        .custom_inbox_prefix("_INBOX.inventory-plan-relay")
        .connection_timeout(Duration::from_secs(2))
        .retry_on_initial_connect(),
    )
    .await
    .map_err(|_| io::Error::other("Could not initialize Inventory plan relay"))?;
    let jetstream = broker.jetstream(DEADLINE);
    let rows = db.collection::<Document>("material_plans");
    rows.create_index(
        IndexModel::builder()
            .keys(doc! {"_id.tenantId": 1, "_id.organizationId": 1, "handoff.pending": 1, "handoff.event_id": 1})
            .options(IndexOptions::builder().partial_filter_expression(doc! {"handoff.pending": true}).build())
            .build(),
    ).await.map_err(|_| io::Error::other("Could not initialize Inventory plan outbox index"))?;
    let idle = Duration::from_millis(300);
    let mut backoff = idle;
    loop {
        let result: Result<bool, &'static str> = async {
            let Some(row) = rows.find_one(doc! {
                "_id.tenantId": scope.tenant.to_string(), "_id.organizationId": scope.organization.to_string(),
                "handoff.pending": true,
            }).sort(doc! {"handoff.event_id": 1}).await.map_err(|_| "read")? else { return Ok(false); };
            let id = row.get_document("_id").map_err(|_| "identity")?;
            let record: PreparedPlan = bson::from_document(row.clone()).map_err(|_| "decode")?;
            let pending = record.handoff.ok_or("handoff")?;
            let event: Envelope<NettedPlan> = serde_json::from_str(&pending.envelope).map_err(|_| "envelope")?;
            if event.tenant_id != Some(scope.tenant) || event.organization_id != Some(scope.organization)
                || event.message_id != pending.event_id || event.payload.plan_id != record.plan.id
                || event.causation_id != Some(record.plan.source_event_id) || event.source_service != SOURCE
            {
                return Err("identity");
            }
            timeout(DEADLINE, jetstream.publish(&event)).await.map_err(|_| "timeout")?.map_err(|_| "ack")?;
            rows.update_one(doc! {
                "_id": id, "handoff.event_id": pending.event_id.to_string(), "handoff.pending": true,
            }, doc! {"$set": {"handoff.pending": false, "handoff.published_at": DateTime::now()}})
                .await.map_err(|_| "mark")?;
            Ok(true)
        }.await;
        match result {
            Ok(true) => backoff = idle,
            Ok(false) => {
                backoff = idle;
                sleep(idle).await;
            }
            Err(stage) => {
                tracing::warn!(stage, "Inventory plan outbox pending; backing off");
                sleep(backoff).await;
                backoff = (backoff * 2).min(DEADLINE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_encodes_a_bounded_fact_without_duplicate_materials() {
        let event: Envelope<NettedPlan> = serde_json::from_str(include_str!(
            "../../../../contracts/examples/nats/inventory-material-plan-netted-v1.json"
        ))
        .unwrap();
        assert!(event.validate().is_ok());
        assert!(serde_json::to_string(&event).unwrap().len() < MAX_PAYLOAD);
        let mut payload = event.payload;
        payload.buy_rows.push(BuyRow {
            item: payload.buy_rows[0].item.clone(),
            unit: "EA".into(),
            pieces: "1".into(),
        });
        assert!(payload.validate().is_err());
    }
}
