//! Inventory runtime plus the preserved trusted, one-shot stock fixture adapter.
use mongodb::{
    Client, ClientSession, Collection, Database,
    bson::{DateTime, Document, doc},
    error::Error,
};
use serde::{Deserialize, Serialize};
use std::{
    env,
    io::{self, Read},
    process::ExitCode,
};
use uuid::Uuid;

mod cancellation;
mod consumer;
mod material_plan;
mod netting;
mod preview;
mod query;
#[cfg(test)]
mod receipt;

#[derive(Deserialize)]
#[serde(tag = "contract", deny_unknown_fields, rename_all_fields = "camelCase")]
enum Command {
    #[serde(rename = "confirmed-stock-line.v1")]
    Confirmed {
        order_id: Uuid,
        line_id: Uuid,
        warehouse_id: Uuid,
        article_id: Uuid,
        unit: Unit,
        quantity: i32,
    },
    #[serde(rename = "cancel-stock-line.v1")]
    Cancel { order_id: Uuid, line_id: Uuid },
}
#[derive(Deserialize)]
enum Unit {
    #[serde(rename = "EA")]
    Each,
}
#[derive(Serialize)]
struct Allocation {
    reserved: i32,
    outstanding: i32,
}

fn line_key(scope: &Document, order_id: Uuid, line_id: Uuid) -> Document {
    let mut key = scope.clone();
    key.extend(doc! {"orderId": order_id.to_string(), "lineId": line_id.to_string()});
    key
}

async fn reserve(
    db: Database,
    line_key: Document,
    stock_key: Document,
    requested: i32,
) -> mongodb::error::Result<Allocation> {
    let stock = db.collection::<Document>("balances");
    let reservations = db.collection::<Document>("reservations");
    let mut session = db.client().start_session().await?;
    session
        .start_transaction()
        .and_run2(async |session| {
            allocate(
                session,
                &stock,
                &reservations,
                &stock_key,
                &line_key,
                requested,
            )
            .await
        })
        .await
}

async fn allocate(
    session: &mut ClientSession,
    stock: &Collection<Document>,
    reservations: &Collection<Document>,
    stock_key: &Document,
    line_key: &Document,
    requested: i32,
) -> mongodb::error::Result<Allocation> {
    let line_filter = doc! { "_id": line_key };
    if let Some(old) = reservations
        .find_one(line_filter)
        .session(&mut *session)
        .await?
    {
        if old.get_document("stock") != Ok(stock_key) || old.get_i32("requested") != Ok(requested) {
            return Err(Error::custom(preview::Rule::ConflictingLine));
        }
        return Ok(Allocation {
            reserved: old.get_i32("reserved").map_err(Error::custom)?,
            outstanding: old.get_i32("outstanding").map_err(Error::custom)?,
        });
    }
    let stock_filter = doc! { "_id": stock_key };
    let balance = stock
        .find_one(stock_filter.clone())
        .session(&mut *session)
        .await?;
    let available = balance
        .as_ref()
        .map(|b| b.get_i32("available"))
        .transpose()
        .map_err(Error::custom)?
        .unwrap_or(0);
    if available < 0 {
        return Err(Error::custom("invalid stock balance"));
    }
    let reserved = requested.min(available);
    let outstanding = requested - reserved;
    if reserved > 0 {
        stock
            .update_one(stock_filter, doc! { "$inc": {"available": -reserved} })
            .session(&mut *session)
            .await?;
    }
    let record = doc! {
        "_id": line_key, "stock": stock_key, "requested": requested,
        "reserved": reserved, "outstanding": outstanding, "schemaVersion": 1,
        "createdAt": DateTime::now(), "actor": "inventory.local-fixture",
    };
    reservations.insert_one(record).session(session).await?;
    Ok(Allocation {
        reserved,
        outstanding,
    })
}

async fn run() -> Result<(), &'static str> {
    let id = |key| {
        env::var(key)
            .ok()
            .and_then(|s| Uuid::parse_str(&s).ok())
            .map(|id| id.to_string())
            .ok_or("missing Inventory scope")
    };
    let scope = doc! {"tenantId":id("INVENTORY_TENANT_ID")?, "organizationId":id("INVENTORY_ORGANIZATION_ID")?};
    let uri = env::var("INVENTORY_MONGO_URI").map_err(|_| "missing Inventory connection")?;
    let mut bytes = Vec::new();
    io::stdin()
        .take(8193)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read fixture")?;
    if bytes.len() > 8192 {
        return Err("fixture too large");
    }
    let command: Command =
        serde_json::from_slice(&bytes).map_err(|_| "invalid Inventory command")?;
    let db = Client::with_uri_str(uri)
        .await
        .map_err(|_| "invalid Inventory connection")?
        .database("inventory");
    let reply = match command {
        Command::Confirmed {order_id, line_id, warehouse_id, article_id, unit: Unit::Each, quantity} => {
            if quantity <= 0 { return Err("quantity must be positive"); }
            let key = line_key(&scope, order_id, line_id);
            let mut stock_key = scope;
            stock_key.extend(doc! { "warehouseId": warehouse_id.to_string(), "articleId": article_id.to_string(), "unit": "EA" });
            let allocation = reserve(db, key, stock_key, quantity).await.map_err(|_| "reservation not acknowledged")?;
            serde_json::to_string(&allocation)
        }
        Command::Cancel {order_id, line_id} => {
            let release = cancellation::cancel(&db, line_key(&scope, order_id, line_id)).await.map_err(|_| "cancellation not acknowledged")?;
            serde_json::to_string(&release)
        }
    }.map_err(|_| "cannot encode result")?;
    println!("{reply}");
    Ok(())
}
#[tokio::main]
async fn main() -> ExitCode {
    let mode = env::args().nth(1);
    if mode.is_some() {
        tracing_subscriber::fmt().json().init();
    }
    let outcome = match mode.as_deref() {
        None => run().await.map_err(io::Error::other),
        Some("serve") => query::serve().await,
        Some("seed-preview") => preview::seed().await,
        Some("provision-consumer") => consumer::provision().await,
        Some("consume") => consumer::consume().await,
        Some("relay-plans") => material_plan::handoff::relay().await,
        Some(_) => Err(io::Error::other("Unknown Inventory role")),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
