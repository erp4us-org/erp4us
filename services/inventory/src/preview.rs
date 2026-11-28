use crate::{allocate, consumer::ConfirmedOrder, line_key};
use erp4us_messaging::Envelope;
use mongodb::{
    Client, Collection, Database,
    bson::{Bson, DateTime, Document, doc},
    error::Error,
};
use std::{env, io};
use uuid::Uuid;

#[derive(Debug)]
pub(super) enum Rule {
    ConflictingLine,
    ConflictingMessage,
    InvalidQuantity,
    UnconfiguredStock,
}

pub(super) fn required(key: &str) -> io::Result<String> {
    env::var(key).map_err(|_| io::Error::other(format!("{key} is required")))
}

fn uuid_env(name: &str) -> io::Result<Uuid> {
    Uuid::parse_str(&required(name)?)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or_else(|| io::Error::other(format!("Invalid {name}")))
}

pub(super) async fn open() -> io::Result<Database> {
    Client::with_uri_str(required("INVENTORY_MONGO_URI")?)
        .await
        .map(|client| client.database("inventory"))
        .map_err(|_| io::Error::other("Could not configure Inventory storage"))
}

#[derive(Clone, Copy)]
pub(super) struct Warehouse {
    pub tenant: Uuid,
    pub organization: Uuid,
    pub id: Uuid,
}

pub(super) fn scoped(tenant: Uuid, organization: Uuid) -> Document {
    doc! { "tenantId": tenant.to_string(), "organizationId": organization.to_string() }
}

impl Warehouse {
    pub(super) fn load() -> io::Result<Self> {
        Ok(Self {
            tenant: uuid_env("INVENTORY_TENANT_ID")?,
            organization: uuid_env("INVENTORY_ORGANIZATION_ID")?,
            id: uuid_env("INVENTORY_WAREHOUSE_ID")?,
        })
    }

    pub(super) fn stock_key(&self, sku: &str, revision: Option<&str>) -> Document {
        let mut scope = scoped(self.tenant, self.organization);
        scope.extend(doc! {
            "warehouseId": self.id.to_string(),
            "catalogSku": sku,
            "productRevision": revision.map_or(Bson::Null, |value| Bson::String(value.into())),
            "unit": "EA",
        });
        scope
    }
}

pub(super) async fn seed() -> io::Result<()> {
    let warehouse = Warehouse::load()?;
    let db = open().await?;
    // Only the explicitly seeded catalog/warehouse pairs are eligible. The
    // NovaMotion starts with no finished modules or subassemblies. A separate
    // stocked product exercises partial finished-goods allocation.
    // Material opening stock is physical on-hand, not outstanding receipts.
    for (sku, revision, opening) in [
        ("NM-1000", Some("A"), 0),
        ("NM-2000", Some("A"), 6),
        ("NM-1100", Some("A"), 0),
        ("NM-1200", Some("A"), 0),
        ("NM-PROFILE", None, 20),
        ("NM-ENDPLATE", None, 8),
        ("NM-SCREW", None, 120),
        ("NM-MOTOR", None, 4),
        ("NM-COUPLING", None, 10),
        ("NM-SPINDLE", None, 0),
        ("NM-RAIL", None, 12),
        ("NM-SENSOR", None, 10),
        ("NM-CABLE", None, 10),
        ("NM-PACK", None, 10),
    ] {
        seed_balance(&db, warehouse.stock_key(sku, revision), opening)
            .await
            .map_err(|_| io::Error::other("Inventory preview seed unavailable"))?;
    }
    Ok(())
}

async fn seed_balance(db: &Database, key: Document, opening: i32) -> mongodb::error::Result<()> {
    let balances = db.collection::<Document>("balances");
    let ledger = db.collection::<Document>("stock_movements");
    let mut session = db.client().start_session().await?;
    session.start_transaction().and_run2(async |session| {
        if balances.find_one(doc! {"_id": &key}).session(&mut *session).await?.is_some() {
            return Ok(());
        }
        balances.insert_one(doc! {"_id": &key, "available": opening, "expected": 0, "schemaVersion": 1})
            .session(&mut *session).await?;
        if opening > 0 {
            ledger.insert_one(doc! {
                "_id": {"stock": &key, "kind": "preview-opening"},
                "quantity": opening, "actor": "inventory.preview-seed", "createdAt": DateTime::now(),
                "schemaVersion": 1,
            }).session(session).await?;
        }
        Ok(())
    }).await
}

pub(super) async fn reserve_confirmed(
    db: &Database,
    warehouse: Warehouse,
    event: &Envelope<ConfirmedOrder>,
) -> mongodb::error::Result<()> {
    let payload = &event.payload;
    let requested = payload
        .quantity()
        .ok_or_else(|| Error::custom(Rule::InvalidQuantity))?;
    let scope = scoped(warehouse.tenant, warehouse.organization);
    let line = line_key(&scope, payload.order_id, payload.line_id);
    let stock = warehouse.stock_key(&payload.sku, Some(&payload.product_revision));
    let mut inbox_key = scope;
    inbox_key.insert("messageId", event.message_id.to_string());
    let intent = doc! { "line": &line, "stock": &stock, "requested": requested };
    let balances = db.collection::<Document>("balances");
    let reservations = db.collection::<Document>("reservations");
    let inbox = db.collection::<Document>("sales_inbox");
    let mut session = db.client().start_session().await?;

    session.start_transaction().and_run2(async |session| {
        if let Some(receipt) = inbox.find_one(doc! {"_id": &inbox_key}).session(&mut *session).await? {
            if receipt.get_document("intent") != Ok(&intent) {
                return Err(Error::custom(Rule::ConflictingMessage));
            }
            return Ok(());
        }
        if balances.find_one(doc! {"_id": &stock}).session(&mut *session).await?.is_none() {
            return Err(Error::custom(Rule::UnconfiguredStock));
        }
        allocate(session, &balances, &reservations, &stock, &line, requested).await?;
        // Preserve the first source even if the producer redelivers the same
        // commercial line under another message ID.
        reservations.update_one(doc! {"_id": &line, "sourceEventId": {"$exists": false}}, doc! {"$set": {
            "sourceEventId": event.message_id.to_string(), "actor": "inventory.sales-confirmed",
        }}).session(&mut *session).await?;
        inbox.insert_one(doc! {
            "_id": &inbox_key, "intent": &intent, "correlationId": event.correlation_id.to_string(),
            "processedAt": DateTime::now(), "schemaVersion": 1,
        }).session(session).await?;
        Ok(())
    }).await
}

pub(super) fn reservations(db: &Database) -> Collection<Document> {
    db.collection("reservations")
}
