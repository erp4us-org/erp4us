use super::{
    Allocation, Binding, Command, LineState, Organization, OriginalBuyRow, PlanRow, PlanRowKey,
    PurchaseLine, ReceiptEffect, Reject, SalesLine, Saved, StockKey, check_replay,
};
use crate::{
    line_key,
    netting::{Item, positive},
    preview::{Warehouse, scoped},
    receipt::{add, subtract},
};
use mongodb::bson::{self, Bson, Document, doc};
use uuid::Uuid;

pub(super) const SCHEMA: i32 = 1;

pub(super) fn sales_key(sales: SalesLine) -> Document {
    line_key(
        &scoped(sales.scope.tenant, sales.scope.organization),
        sales.order,
        sales.line,
    )
}

pub(super) fn stock_key(stock: &StockKey) -> Document {
    Warehouse {
        tenant: stock.scope.tenant,
        organization: stock.scope.organization,
        id: stock.warehouse,
    }
    .stock_key(&stock.item.sku, stock.item.revision.as_deref())
}

fn plan_key(scope: Organization, plan: Uuid) -> Document {
    let mut key = scoped(scope.tenant, scope.organization);
    key.insert("planId", plan.to_string());
    key
}

pub(super) fn row_key(row: &PlanRowKey) -> Document {
    let mut key = plan_key(row.sales.scope, row.plan);
    key.extend(doc! {"sku": &row.stock.item.sku, "revision": &row.stock.item.revision});
    key
}

pub(super) fn purchase_key(scope: Organization, plan: Uuid, purchase: PurchaseLine) -> Document {
    let mut key = plan_key(scope, plan);
    key.extend(doc! {
        "purchaseOrderId": purchase.order.to_string(), "purchaseLineId": purchase.line.to_string(),
    });
    key
}

pub(super) fn receipt_key(command: Command) -> Document {
    let (scope, id) = command.replay_key();
    let mut key = scoped(scope.tenant, scope.organization);
    key.insert("receiptId", id.to_string());
    key
}

fn field<T>(value: Result<T, impl std::fmt::Debug>) -> Result<T, Reject> {
    value.map_err(|_| Reject::InvalidState)
}

fn id(record: &Document, name: &str) -> Result<Uuid, Reject> {
    let text = field(record.get_str(name))?;
    Uuid::try_parse(text)
        .ok()
        .filter(|id| !id.is_nil() && id.to_string() == text)
        .ok_or(Reject::InvalidState)
}

fn item(record: &Document) -> Result<Item, Reject> {
    let item = Item {
        sku: field(record.get_str("sku"))?.into(),
        revision: match record.get("revision") {
            Some(Bson::Null) => None,
            Some(Bson::String(revision)) => Some(revision.clone()),
            _ => return Err(Reject::InvalidState),
        },
    };
    item.valid().then_some(item).ok_or(Reject::InvalidState)
}

fn pieces(record: &Document, name: &str) -> Result<i32, Reject> {
    match field(record.get_str(name))? {
        "0" => Ok(0),
        quantity => positive(quantity).ok_or(Reject::InvalidState),
    }
}

fn version(document: &Document) -> Result<(), Reject> {
    (document.get_i32("schemaVersion") == Ok(SCHEMA))
        .then_some(())
        .ok_or(Reject::InvalidState)
}

/// Inspect the stored netting snapshot, never availability, for the original buy row.
/// A live reservation still linked to this exact snapshot is the preview's waiting guard.
pub(super) fn original(
    record: &Document,
    sales: SalesLine,
    issued_plan: Uuid,
    issued_item: &Item,
) -> Result<(PlanRow, Document), Reject> {
    version(record)?;
    let key = sales_key(sales);
    let plan = field(record.get_document("plan"))?;
    let plan_id = id(plan, "id")?;
    let warehouse = id(plan, "warehouse_id")?;
    if record.get_document("_id") != Ok(&key)
        || id(plan, "order_id")? != sales.order
        || id(plan, "line_id")? != sales.line
        || plan.get_str("unit") != Ok("EA")
    {
        return Err(Reject::InvalidState);
    }
    if plan_id != issued_plan {
        return Err(Reject::InvalidSource);
    }
    let mut matched = None;
    for row in field(plan.get_array("materials"))? {
        let row = row.as_document().ok_or(Reject::InvalidState)?;
        if item(row)? == *issued_item {
            let quantities = OriginalBuyRow {
                requested: pieces(row, "requested")?,
                held: pieces(row, "reserved")?,
                buy: pieces(row, "uncovered")?,
            };
            if matched.replace(quantities).is_some() {
                return Err(Reject::InvalidState);
            }
        }
    }
    let source_stock = StockKey {
        scope: sales.scope,
        warehouse,
        item: Item {
            sku: field(plan.get_str("sku"))?.into(),
            revision: Some(field(plan.get_str("product_revision"))?.into()),
        },
    };
    let outstanding = pieces(plan, "pieces")?;
    if !source_stock.item.valid() || outstanding == 0 {
        return Err(Reject::InvalidState);
    }
    let guard = doc! {
        "_id": key, "schemaVersion": SCHEMA, "materialPlanId": plan_id.to_string(),
        "sourceEventId": id(plan, "source_event_id")?.to_string(), "stock": stock_key(&source_stock),
        "outstanding": outstanding, "cancellation": {"$exists": false},
    };
    Ok((
        PlanRow {
            key: PlanRowKey {
                sales,
                plan: plan_id,
                stock: StockKey {
                    scope: sales.scope,
                    warehouse,
                    item: issued_item.clone(),
                },
            },
            original: matched.ok_or(Reject::PlanIneligible)?,
        },
        guard,
    ))
}

pub(super) fn is_waiting(row: &Document, guard: &Document) -> bool {
    // Mongo equality can also match an element of an array. Require the
    // exact typed scalars and stock document, not just a matching filter.
    !row.contains_key("cancellation")
        && [
            "_id",
            "schemaVersion",
            "materialPlanId",
            "sourceEventId",
            "stock",
            "outstanding",
        ]
        .into_iter()
        .all(|key| {
            guard
                .get(key)
                .is_some_and(|expected| row.get(key) == Some(expected))
        })
}

pub(super) fn line_record(effect: &ReceiptEffect) -> Result<Document, Reject> {
    let binding = &effect.line.binding;
    let row = &binding.row;
    let key = &row.key;
    Ok(doc! {
        "_id": row_key(key),
        "schemaVersion": SCHEMA,
        "purchaseKey": purchase_key(key.sales.scope, key.plan, binding.purchase),
        "reservationKey": sales_key(key.sales),
        "stockKey": stock_key(&key.stock),
        "original": {
            "requested": row.original.requested, "held": row.original.held, "buy": row.original.buy,
        },
        "allocation": field(bson::to_document(&Allocation::from_effect(effect)))?,
    })
}

pub(super) fn read_line(document: &Document) -> Result<(LineState, Allocation), Reject> {
    version(document)?;
    let key = field(document.get_document("_id"))?;
    let scope = Organization {
        tenant: id(key, "tenantId")?,
        organization: id(key, "organizationId")?,
    };
    let plan = id(key, "planId")?;
    let reservation = field(document.get_document("reservationKey"))?;
    let sales = SalesLine {
        scope,
        order: id(reservation, "orderId")?,
        line: id(reservation, "lineId")?,
    };
    let stock = field(document.get_document("stockKey"))?;
    let row_key_value = PlanRowKey {
        sales,
        plan,
        stock: StockKey {
            scope,
            warehouse: id(stock, "warehouseId")?,
            item: item(key)?,
        },
    };
    let purchase_selector = field(document.get_document("purchaseKey"))?;
    let purchase = PurchaseLine {
        order: id(purchase_selector, "purchaseOrderId")?,
        line: id(purchase_selector, "purchaseLineId")?,
    };
    if row_key(&row_key_value) != *key
        || sales_key(sales) != *reservation
        || stock_key(&row_key_value.stock) != *stock
        || purchase_key(scope, plan, purchase) != *purchase_selector
    {
        return Err(Reject::InvalidState);
    }
    let snapshot = field(document.get_document("original"))?;
    let original = OriginalBuyRow {
        requested: field(snapshot.get_i32("requested"))?,
        held: field(snapshot.get_i32("held"))?,
        buy: field(snapshot.get_i32("buy"))?,
    };
    let allocation: Allocation = field(bson::from_document(
        field(document.get_document("allocation"))?.clone(),
    ))?;
    if add(original.held, original.buy) != Some(original.requested)
        || !(1..=original.buy).contains(&allocation.allocated)
        || !(1..=allocation.allocated).contains(&allocation.sequence)
        || subtract(original.buy, allocation.allocated) != Some(allocation.open)
        || add(original.held, allocation.allocated) != Some(allocation.held)
        || subtract(original.requested, allocation.held) != Some(allocation.shortage)
    {
        return Err(Reject::InvalidState);
    }
    Ok((
        LineState {
            binding: Binding {
                row: PlanRow {
                    key: row_key_value,
                    original,
                },
                purchase,
            },
            allocated: allocation.allocated,
            sequence: allocation.sequence,
        },
        allocation,
    ))
}

pub(super) fn receipt_record(saved: &Saved, line: &Document, envelope: String) -> Document {
    let command = saved.effect.command;
    doc! {
        "_id": receipt_key(command), "schemaVersion": SCHEMA,
        "actorId": command.actor.to_string(), "pieces": command.pieces,
        "heldPieces": saved.effect.hold_increase(), "postedAt": saved.posted_at,
        "lineAtPosting": line, "outbox": {
            "messageId": saved.message_id.to_string(), "pending": true, "envelope": envelope,
        },
    }
}

pub(super) fn read_saved(command: Command, document: &Document) -> Result<Saved, Reject> {
    version(document)?;
    if document.get_document("_id") != Ok(&receipt_key(command)) {
        return Err(Reject::InvalidState);
    }
    let actor = id(document, "actorId")?;
    if actor != command.actor {
        return Err(Reject::ActorMismatch);
    }
    let (line, allocation) = read_line(field(document.get_document("lineAtPosting"))?)?;
    let pieces = field(document.get_i32("pieces"))?;
    let message_id = id(field(document.get_document("outbox"))?, "messageId")?;
    if line.binding.row.key.sales.scope != command.sales.scope
        || !(1..=allocation.allocated).contains(&pieces)
        || document.get_i32("heldPieces") != Ok(pieces)
        || message_id == command.receipt_id
    {
        return Err(Reject::InvalidState);
    }
    let effect = ReceiptEffect {
        command: Command {
            sales: line.binding.row.key.sales,
            purchase: line.binding.purchase,
            receipt_id: command.receipt_id,
            actor,
            pieces,
        },
        line,
        line_open: allocation.open,
        plan_held: allocation.held,
        plan_remaining_shortage: allocation.shortage,
    };
    check_replay(command, Some(&effect))?;
    Ok(Saved {
        effect,
        posted_at: *field(document.get_datetime("postedAt"))?,
        message_id,
    })
}

pub(super) fn movement(saved: &Saved) -> Document {
    let command = saved.effect.command;
    let row = &saved.effect.line.binding.row.key;
    doc! {
        "_id": {
            "stock": stock_key(&row.stock), "kind": "purchase-receipt",
            "receiptId": command.receipt_id.to_string(),
        },
        "quantity": saved.effect.physical_increase(), "actor": command.actor.to_string(),
        "createdAt": saved.posted_at, "planId": row.plan.to_string(), "schemaVersion": SCHEMA,
    }
}
