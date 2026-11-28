use super::*;
use crate::{netting::Item, preview::Warehouse};
use erp4us_messaging::{Contract, Delivery, Envelope, stream};
use mongodb::{Client, bson, bson::Bson};
use serde_json::{Value, json};

#[path = "../../../../../tests/integration/receipt_transport.rs"]
mod transport;

const SCOPE: erp4us_identity::Scope = erp4us_identity::Scope {
    tenant: Uuid::from_u128(1),
    organization: Uuid::from_u128(2),
    actor: Uuid::from_u128(3),
};
const ORDER: Uuid = Uuid::from_u128(4);
const LINE: Uuid = Uuid::from_u128(5);
const PLAN: Uuid = Uuid::from_u128(6);
const WAREHOUSE: Uuid = Uuid::from_u128(7);
const SOURCE_EVENT: Uuid = Uuid::from_u128(8);
const PURCHASE: PurchaseLine = PurchaseLine {
    order: Uuid::from_u128(9),
    line: Uuid::from_u128(10),
};

fn command(scope: erp4us_identity::Scope, id: u128, pieces: &str) -> Command {
    Command::new(scope, ORDER, LINE, PURCHASE, Uuid::from_u128(id), pieces).unwrap()
}

fn stock(command: Command, sku: &str) -> StockKey {
    StockKey {
        scope: command.sales.scope,
        warehouse: WAREHOUSE,
        item: Item {
            sku: sku.to_owned(),
            revision: None,
        },
    }
}

fn source(command: Command, item: &Item) -> IssuedLine<'_> {
    IssuedLine {
        scope: command.sales.scope,
        actor: command.actor,
        plan: PLAN,
        purchase: command.purchase,
        item,
        unit: "EA",
        ordered: 12,
    }
}

fn plan_record(command: Command) -> Document {
    doc! {
        "_id": sales_key(command.sales), "schemaVersion": SCHEMA,
        "plan": {
            "id": PLAN.to_string(), "order_id": ORDER.to_string(), "line_id": LINE.to_string(),
            "warehouse_id": WAREHOUSE.to_string(), "source_event_id": SOURCE_EVENT.to_string(),
            "unit": "EA", "sku": "NM-1000", "product_revision": "A", "pieces": "10",
            "materials": [
                {"sku": "NM-ENDPLATE", "revision": Bson::Null, "requested": "20", "reserved": "8", "uncovered": "12"},
                {"sku": "NM-SCREW", "revision": Bson::Null, "requested": "12", "reserved": "0", "uncovered": "12"},
            ],
        },
        "handoff": {"pending": false, "envelope": "original handoff stays unchanged"},
    }
}

async fn seed_waiting_endplates(db: &Database, command: Command, component: &Document) {
    db.collection::<Document>("material_plans")
        .insert_one(plan_record(command))
        .await
        .unwrap();
    db.collection::<Document>("reservations")
        .insert_one(doc! {
            "_id": sales_key(command.sales), "schemaVersion": SCHEMA, "materialPlanId": PLAN.to_string(),
            "sourceEventId": SOURCE_EVENT.to_string(), "reserved": 0, "outstanding": 10,
            "stock": Warehouse { tenant: SCOPE.tenant, organization: SCOPE.organization, id: WAREHOUSE }
                .stock_key("NM-1000", Some("A")),
        })
        .await
        .unwrap();
    db.collection::<Document>("balances")
        .insert_one(doc! {"_id": component, "schemaVersion": SCHEMA, "available": 0, "expected": 0})
        .await
        .unwrap();
    db.collection::<Document>(LEDGER)
        .insert_one(doc! {
            "_id": {"stock": component, "kind": "preview-opening"},
            "schemaVersion": SCHEMA, "quantity": 8,
        })
        .await
        .unwrap();
}

fn saved_fixture() -> Saved {
    let command = command(SCOPE, 11, "6");
    let stock = stock(command, "NM-ENDPLATE");
    let issued = source(command, &stock.item);
    let (plan, _) =
        records::original(&plan_record(command), command.sales, PLAN, &stock.item).unwrap();
    Saved {
        effect: post_new(
            command,
            &plan,
            &CurrentRow {
                key: plan.key.clone(),
                eligibility: Eligibility::Waiting,
                purchase: None,
                held: 8,
            },
            &issued,
            None,
            Some(&stock),
        )
        .unwrap(),
        posted_at: DateTime::from_millis(1_796_000_000_001),
        message_id: Uuid::from_u128(20),
    }
}

#[test]
fn bsons_round_trip_original_intent_time_and_scoped_binding() {
    let saved = saved_fixture();
    let command = saved.effect.command;
    let line = records::line_record(&saved.effect).unwrap();
    let encoded = records::receipt_record(
        &saved,
        &line,
        fact::encode(&saved, ActorKind::User).unwrap(),
    );
    let doc: Document = bson::from_slice(&bson::to_vec(&encoded).unwrap()).unwrap();
    assert_eq!(records::read_saved(command, &doc).unwrap(), saved);
    assert_eq!(
        doc.get_document("_id").unwrap(),
        &doc! {"tenantId": SCOPE.tenant.to_string(), "organizationId": SCOPE.organization.to_string(),
        "receiptId": command.receipt_id.to_string()}
    );
    let (state, allocation) = records::read_line(&line).unwrap();
    assert_eq!(state, saved.effect.line);
    assert_eq!(
        (
            allocation.allocated,
            allocation.open,
            allocation.held,
            allocation.shortage
        ),
        (6, 6, 14, 6)
    );
    assert_eq!(state.binding.row.original.held, 8);
    assert_eq!(state.binding.row.original.buy, 12);
    let key = line.get_document("_id").unwrap();
    assert_eq!(key.get("revision"), Some(&Bson::Null));
    assert_eq!(key.get_str("planId"), Ok(PLAN.to_string().as_str()));
    assert_eq!(purchase_index().options.unwrap().unique, Some(true));

    let movement = records::movement(&saved);
    let movement_key = movement.get_document("_id").unwrap();
    assert_eq!(movement_key.get_str("kind"), Ok("purchase-receipt"));
    assert_eq!(
        movement_key.get_document("stock").unwrap(),
        line.get_document("stockKey").unwrap()
    );
    assert_eq!(movement.get_i32("quantity"), Ok(6));
    assert_eq!(doc.get_i32("heldPieces"), Ok(6));
    assert_eq!(
        doc.get_datetime("postedAt"),
        movement.get_datetime("createdAt")
    );

    let mut different_actor = command;
    different_actor.actor = Uuid::from_u128(50);
    let mut changed_intent = command;
    changed_intent.sales.line = Uuid::from_u128(51);
    assert_eq!(
        records::read_saved(different_actor, &doc),
        Err(Reject::ActorMismatch)
    );
    assert_eq!(
        records::read_saved(changed_intent, &doc),
        Err(Reject::IntentChanged)
    );
    for foreign in [
        erp4us_identity::Scope {
            tenant: Uuid::from_u128(52),
            ..SCOPE
        },
        erp4us_identity::Scope {
            organization: Uuid::from_u128(53),
            ..SCOPE
        },
    ] {
        let other = Command::new(foreign, ORDER, LINE, PURCHASE, command.receipt_id, "6").unwrap();
        assert_ne!(receipt_key(other), receipt_key(command));
        assert_eq!(records::read_saved(other, &doc), Err(Reject::InvalidState));
    }
}

#[test]
fn source_row_lookup_requires_immutable_sales_pin_explicit_revision_and_ea() {
    let command = command(SCOPE, 11, "6");
    let item = stock(command, "NM-ENDPLATE").item;
    let plan = plan_record(command);
    let (_, waiting) = records::original(&plan, command.sales, PLAN, &item).unwrap();
    assert_eq!(waiting.get_document("_id"), Ok(&sales_key(command.sales)));
    assert_eq!(
        waiting.get_str("materialPlanId"),
        Ok(PLAN.to_string().as_str())
    );
    assert_eq!(waiting.get_i32("outstanding"), Ok(10));
    assert_eq!(
        waiting.get_str("sourceEventId"),
        Ok(SOURCE_EVENT.to_string().as_str())
    );
    let mut reservation = waiting;
    reservation.remove("cancellation");
    let (_, guard) = records::original(&plan, command.sales, PLAN, &item).unwrap();
    assert!(records::is_waiting(&reservation, &guard));
    reservation.insert("outstanding", vec![10]);
    assert!(!records::is_waiting(&reservation, &guard));
    reservation.insert("outstanding", 10);
    reservation.insert("cancellation", Bson::Null);
    assert!(!records::is_waiting(&reservation, &guard));
    assert_eq!(
        records::original(&plan, command.sales, Uuid::from_u128(99), &item),
        Err(Reject::InvalidSource)
    );
    assert_eq!(
        records::original(
            &plan,
            command.sales,
            PLAN,
            &Item {
                revision: Some("A".into()),
                ..item.clone()
            }
        ),
        Err(Reject::PlanIneligible)
    );
    for field in ["unit", "order_id", "line_id"] {
        let mut malformed = plan.clone();
        malformed
            .get_document_mut("plan")
            .unwrap()
            .insert(field, "wrong");
        assert_eq!(
            records::original(&malformed, command.sales, PLAN, &item),
            Err(Reject::InvalidState)
        );
    }
    let mut missing_revision = plan;
    missing_revision
        .get_document_mut("plan")
        .unwrap()
        .get_array_mut("materials")
        .unwrap()[0]
        .as_document_mut()
        .unwrap()
        .remove("revision");
    assert_eq!(
        records::original(&missing_revision, command.sales, PLAN, &item),
        Err(Reject::InvalidState)
    );
}

#[test]
fn pending_wire_fact_is_a_new_root_and_excludes_internal_fields() {
    let saved = saved_fixture();
    let example: Value = serde_json::from_str(include_str!(
        "../../../../../contracts/examples/nats/inventory-purchase-receipt-posted-v1.json"
    ))
    .unwrap();
    let reference: Envelope<fact::Posted> =
        serde_json::from_value(example["envelope"].clone()).unwrap();
    reference.validate().unwrap();

    let wire = fact::encode(&saved, ActorKind::Agent).unwrap();
    let event: Envelope<fact::Posted> = serde_json::from_str(&wire).unwrap();
    event.validate().unwrap();
    assert_eq!(event.message_id, saved.message_id);
    assert_eq!(event.correlation_id, event.message_id);
    assert_eq!(event.source_service, "erp4us.inventory");
    assert_eq!(event.tenant_id, Some(SCOPE.tenant));
    assert_eq!(event.organization_id, Some(SCOPE.organization));
    let actor = event.actor.unwrap();
    assert_eq!(actor.id, SCOPE.actor.to_string());
    assert_eq!(actor.kind, "agent");
    let wire: Value = serde_json::from_str(&wire).unwrap();
    assert!(wire.get("causationId").is_none());
    assert_eq!(
        wire["payload"],
        json!({
            "receiptId": saved.effect.command.receipt_id,
            "planId": PLAN, "purchaseOrderId": PURCHASE.order, "purchaseLineId": PURCHASE.line,
            "sku": "NM-ENDPLATE", "revision": null, "unit": "EA", "pieces": "6", "receiptSequence": 1
        })
    );
    let mut invalid = wire;
    invalid["payload"]
        .as_object_mut()
        .unwrap()
        .remove("revision");
    assert!(serde_json::from_value::<Envelope<fact::Posted>>(invalid).is_err());
}

#[tokio::test]
async fn posting_deadline_keeps_an_in_flight_task_alive_and_bounded() {
    let slots = Arc::new(Semaphore::new(1));
    let (release, work) = tokio::sync::oneshot::channel();
    let operation = async {
        work.await.map_err(|_| Failure::Unavailable)?;
        Ok::<_, Failure>(())
    };
    assert_eq!(
        bounded_post(&slots, Instant::now() + Duration::from_millis(5), operation).await,
        Err(Failure::Unavailable)
    );
    assert_eq!(slots.available_permits(), 0);
    assert_eq!(
        bounded_post(&slots, Instant::now() + Duration::from_secs(1), async {
            Ok::<_, Failure>(())
        })
        .await,
        Err(Failure::Unavailable)
    );
    release.send(()).unwrap();
    let _slot = timeout(Duration::from_secs(1), slots.acquire())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
#[ignore = "requires INVENTORY_RECEIPT_TEST_MONGO_URI for a dedicated disposable replica set"]
async fn mongo_competing_lots_and_rollback_keep_all_effects_atomic() {
    let uri = std::env::var("INVENTORY_RECEIPT_TEST_MONGO_URI").expect("dedicated test Mongo URI");
    let client = Client::with_uri_str(&uri)
        .await
        .expect("valid test Mongo settings");
    let db = client.database(&format!(
        "inventory_receipt_test_{}",
        Uuid::new_v4().simple()
    ));
    let store = Store::open(&db).await.unwrap();
    let plans = db.collection::<Document>("material_plans");
    let reservations = db.collection::<Document>("reservations");
    let balances = db.collection::<Document>("balances");
    let ledger = db.collection::<Document>(LEDGER);
    let receipts = db.collection::<Document>(RECEIPTS);
    let first = command(SCOPE, 11, "7");
    let competing = command(SCOPE, 12, "7");
    let final_lot = command(SCOPE, 13, "5");
    let destination = stock(first, "NM-ENDPLATE");
    let component = stock_key(&destination);
    let screws = stock(first, "NM-SCREW");
    let issued = source(first, &destination.item);
    let historical = plan_record(first);
    let sales = sales_key(first.sales);
    seed_waiting_endplates(&db, first, &component).await;
    balances
        .insert_one(doc! {"_id": stock_key(&screws), "schemaVersion": SCHEMA, "available": 0, "expected": 0})
        .await
        .unwrap();
    assert_eq!(store.replay(first).await.unwrap(), None);
    assert_eq!(
        store.allocation(first.sales, PLAN, PURCHASE).await.unwrap(),
        None
    );
    let (a, b) = tokio::join!(
        store.post(first, &issued, &destination, ActorKind::User),
        store.post(competing, &issued, &destination, ActorKind::User),
    );
    let original = match (a, b) {
        (Ok(Post::Created(saved)), Err(Failure::Rejected(Reject::OverReceipt)))
        | (Err(Failure::Rejected(Reject::OverReceipt)), Ok(Post::Created(saved))) => saved,
        (a, b) => panic!("expected exactly one competing lot: {a:?}, {b:?}"),
    };
    let winner = original.effect.command;
    let another = PurchaseLine {
        order: PURCHASE.order,
        line: Uuid::from_u128(40),
    };
    let other_line = Command {
        purchase: another,
        ..final_lot
    };
    let other_issued = source(other_line, &destination.item);
    assert_eq!(
        store
            .post(other_line, &other_issued, &destination, ActorKind::User)
            .await,
        Err(Failure::Rejected(Reject::PlanIneligible))
    );
    let other_row = source(final_lot, &screws.item);
    assert_eq!(
        store
            .post(final_lot, &other_row, &screws, ActorKind::User)
            .await,
        Err(Failure::Rejected(Reject::PlanIneligible))
    );
    let before = store
        .allocation(first.sales, PLAN, PURCHASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((before.sequence, before.allocated, before.open), (1, 7, 5));
    let before_balance = balances
        .find_one(doc! {"_id": &component})
        .await
        .unwrap()
        .unwrap();
    let before_reservation = reservations
        .find_one(doc! {"_id": &sales})
        .await
        .unwrap()
        .unwrap();

    // Make the final write fail after staged guards, allocation, receipt, and outbox.
    let conflict = doc! {"stock": &component, "kind": "purchase-receipt", "receiptId": final_lot.receipt_id.to_string()};
    ledger
        .insert_one(doc! {"_id": &conflict, "forcedTestConflict": true})
        .await
        .unwrap();
    assert_eq!(
        store
            .post(final_lot, &issued, &destination, ActorKind::User)
            .await,
        Err(Failure::Unavailable)
    );
    assert_eq!(store.replay(final_lot).await.unwrap(), None);
    assert_eq!(receipts.count_documents(doc! {}).await.unwrap(), 1);
    assert_eq!(
        store.allocation(first.sales, PLAN, PURCHASE).await.unwrap(),
        Some(before)
    );
    assert_eq!(
        balances.find_one(doc! {"_id": &component}).await.unwrap(),
        Some(before_balance)
    );
    assert_eq!(
        reservations.find_one(doc! {"_id": &sales}).await.unwrap(),
        Some(before_reservation)
    );
    assert_eq!(
        ledger
            .delete_one(doc! {"_id": &conflict, "forcedTestConflict": true})
            .await
            .unwrap()
            .deleted_count,
        1
    );

    // A once-verified owner/source and destination cannot override a deactivated reservation.
    reservations
        .update_one(doc! {"_id": &sales}, doc! {"$set": {"outstanding": 0}})
        .await
        .unwrap();
    assert_eq!(
        store
            .post(final_lot, &issued, &destination, ActorKind::User)
            .await,
        Err(Failure::Rejected(Reject::PlanIneligible))
    );
    reservations
        .update_one(doc! {"_id": &sales}, doc! {"$set": {"outstanding": 10}})
        .await
        .unwrap();
    assert!(matches!(
        store
            .post(final_lot, &issued, &destination, ActorKind::User)
            .await
            .unwrap(),
        Post::Created(_)
    ));

    let allocation = store
        .allocation(first.sales, PLAN, PURCHASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            allocation.sequence,
            allocation.allocated,
            allocation.open,
            allocation.held
        ),
        (2, 12, 0, 20)
    );
    let closed = command(SCOPE, 14, "1");
    reservations
        .update_one(doc! {"_id": &sales}, doc! {"$set": {"outstanding": 0}})
        .await
        .unwrap();
    assert_eq!(
        store
            .post(closed, &issued, &destination, ActorKind::User)
            .await,
        Err(Failure::Rejected(Reject::OverReceipt))
    );
    assert_eq!(
        store
            .replay(Command {
                pieces: 6,
                ..winner
            })
            .await,
        Err(Failure::Rejected(Reject::IntentChanged))
    );
    assert_eq!(
        store
            .replay(Command {
                actor: Uuid::from_u128(99),
                ..winner
            })
            .await,
        Err(Failure::Rejected(Reject::ActorMismatch))
    );
    assert!(
        store
            .replay(command(
                erp4us_identity::Scope {
                    organization: Uuid::from_u128(99),
                    ..SCOPE
                },
                winner.receipt_id.as_u128(),
                "7"
            ))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .post(winner, &other_row, &screws, ActorKind::User)
            .await
            .unwrap(),
        Post::Replay(original)
    );
    assert_eq!(
        balances
            .find_one(doc! {"_id": &component})
            .await
            .unwrap()
            .unwrap()
            .get_i32("available"),
        Ok(0)
    );
    assert_eq!(
        plans.find_one(doc! {"_id": &sales}).await.unwrap(),
        Some(historical)
    );
    assert_eq!(
        ledger
            .count_documents(doc! {"_id.kind": "purchase-receipt"})
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        receipts
            .count_documents(doc! {"outbox.pending": true})
            .await
            .unwrap(),
        2
    );
    db.drop().await.unwrap();
}

#[tokio::test]
#[ignore = "requires disposable loopback Mongo replica set and empty JetStream; run before Procurement transport test"]
async fn receipt_transport_publish_committed_inventory_outbox() {
    let (db, jetstream) = transport::connect("inventory").await;
    let ops = jetstream.operations();
    assert_eq!(
        ops.query_account().await.unwrap().streams,
        0,
        "the proof must start with an empty, disposable JetStream"
    );
    let store = Store::open(&db).await.unwrap();
    let first = command(SCOPE, 11, "6");
    let second = command(SCOPE, 12, "6");
    let destination = stock(first, "NM-ENDPLATE");
    let issued = source(first, &destination.item);
    seed_waiting_endplates(&db, first, &stock_key(&destination)).await;
    let mut committed = Vec::new();
    for command in [first, second] {
        let Post::Created(saved) = store
            .post(command, &issued, &destination, ActorKind::User)
            .await
            .unwrap()
        else {
            panic!("expected a new committed lot");
        };
        let row = store
            .majority_read(RECEIPTS)
            .find_one(doc! {
                "_id": receipt_key(command), "outbox.pending": true,
                "outbox.messageId": saved.message_id.to_string(),
            })
            .projection(doc! {"_id": 0, "outbox.envelope": 1})
            .await
            .unwrap()
            .unwrap();
        let body = row
            .get_document("outbox")
            .unwrap()
            .get_str("envelope")
            .unwrap();
        let event: Envelope<fact::Posted> = serde_json::from_str(body).unwrap();
        assert_eq!(event.message_id, saved.message_id);
        // JetStream::publish must send the same body, not a reconstructed fixture.
        assert_eq!(serde_json::to_string(&event).unwrap(), body);
        committed.push(event);
    }
    db.drop().await.unwrap();

    let Delivery::JetStream(name) = fact::Posted::DELIVERY else {
        panic!("the owner contract requires JetStream");
    };
    ops.create_stream(stream::Config {
        name: name.into(),
        description: Some(transport::STREAM_MARKER.into()),
        subjects: vec![committed[0].subject().unwrap()],
        storage: stream::StorageType::Memory,
        max_messages: 3,
        max_age: Duration::from_secs(900),
        max_message_size: 8 * 1024,
        discard: stream::DiscardPolicy::New,
        ..Default::default()
    })
    .await
    .unwrap();
    // Deliver the second physical lot first. Separate test processes share only this stream.
    for (sequence, event) in (1..).zip(committed.iter().rev()) {
        let ack = jetstream.publish(event).await.unwrap();
        assert_eq!(ack.sequence, sequence);
        assert!(!ack.duplicate);
    }
}
