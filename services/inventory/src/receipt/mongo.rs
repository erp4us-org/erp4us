//! Test-gated Inventory persistence for the proposed waiting-plan receipt.
//!
//! Keep fixed-size line counters separate from the scoped, append-only receipt results:
//! a million one-piece lots must not grow one Mongo document. Each immutable result
//! includes its historical allocation and a pending envelope, so there is no third
//! receipt collection. The Inventory movement ledger remains the physical record;
//! the received quantity is held in full and never added to free `available`.
//!
//! Future callers must authorize before replay, check it before any owner lookup,
//! verify the owner tuple with the same actor/scope, and verify the destination. This
//! adapter has no owner, broker or HTTP connection. A future release/consumption path
//! must serialize on the same reservation/line and account for these additional holds.

use super::{
    Binding, Command, CurrentRow, Eligibility, IssuedLine, LineState, Organization, OriginalBuyRow,
    PlanRow, PlanRowKey, PurchaseLine, ReceiptEffect, Reject, SalesLine, StockKey, check_replay,
    post_new,
};
use mongodb::{
    ClientSession, Collection, Database, IndexModel,
    bson::{DateTime, Document, doc},
    error::{
        Error as MongoError, ErrorKind, TRANSIENT_TRANSACTION_ERROR,
        UNKNOWN_TRANSACTION_COMMIT_RESULT, WriteFailure,
    },
    options::{
        Acknowledgment, CollectionOptions, IndexOptions, ReadConcern, ReadPreference,
        SelectionCriteria, WriteConcern,
    },
};
use serde::{Deserialize, Serialize};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{
    sync::Semaphore,
    time::{Instant, sleep, timeout, timeout_at},
};
use uuid::Uuid;

mod fact;
mod records;
mod tests;

use fact::ActorKind;
use records::{SCHEMA, purchase_key, receipt_key, row_key, sales_key, stock_key};

const LINES: &str = "purchase_receipt_lines";
const RECEIPTS: &str = "purchase_receipts";
const LEDGER: &str = "stock_movements";
const DB_LIMIT: Duration = Duration::from_secs(5);
const COMMIT_LIMIT: Duration = Duration::from_secs(2);
const MAX_ATTEMPTS: u32 = 5;
const MAX_POSTS: usize = 16;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Allocation {
    sequence: i32,
    allocated: i32,
    open: i32,
    held: i32,
    shortage: i32,
}

impl Allocation {
    fn from_effect(effect: &ReceiptEffect) -> Self {
        Self {
            sequence: effect.line.sequence,
            allocated: effect.line.allocated,
            open: effect.line_open,
            held: effect.plan_held,
            shortage: effect.plan_remaining_shortage,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Saved {
    effect: ReceiptEffect,
    posted_at: DateTime,
    message_id: Uuid,
}

#[derive(Debug, PartialEq, Eq)]
enum Post {
    Created(Saved),
    Replay(Saved),
}

#[derive(Debug, PartialEq, Eq)]
enum Failure {
    Rejected(Reject),
    /// Includes unknown commit outcome. Retry the same command and receipt ID.
    Unavailable,
}

enum Step {
    Rejected(Reject),
    Database(MongoError),
}

impl From<Reject> for Step {
    fn from(error: Reject) -> Self {
        Self::Rejected(error)
    }
}

impl From<MongoError> for Step {
    fn from(error: MongoError) -> Self {
        Self::Database(error)
    }
}

fn primary() -> SelectionCriteria {
    SelectionCriteria::ReadPreference(ReadPreference::Primary)
}

fn majority_write() -> WriteConcern {
    WriteConcern::builder()
        .w(Acknowledgment::Majority)
        .w_timeout(COMMIT_LIMIT)
        .build()
}

fn purchase_index() -> IndexModel {
    IndexModel::builder()
        .keys(doc! {"purchaseKey": 1})
        .options(
            IndexOptions::builder()
                .name("one_row_per_scoped_purchase_line".to_owned())
                .unique(true)
                .build(),
        )
        .build()
}

#[derive(Clone)]
struct Store {
    db: Database,
    posting_slots: Arc<Semaphore>,
}

impl Store {
    /// Mongo's `_id` indexes cover the scoped command and scoped plan row. The
    /// secondary unique index also prevents one purchase tuple binding two rows.
    async fn open(db: &Database) -> Result<Self, Failure> {
        timeout(DB_LIMIT, async {
            for name in [LINES, RECEIPTS, LEDGER] {
                match db
                    .create_collection(name)
                    .write_concern(majority_write())
                    .await
                {
                    // Existing collections already have their builtin _id indexes.
                    Err(error) if matches!(&*error.kind, ErrorKind::Command(e) if e.code == 48) => {
                    }
                    result => result.map_err(|_| Failure::Unavailable)?,
                }
            }
            db.collection::<Document>(LINES)
                .create_index(purchase_index())
                .write_concern(majority_write())
                .await
                .map_err(|_| Failure::Unavailable)?;
            Ok(Self {
                db: db.clone(),
                posting_slots: Arc::new(Semaphore::new(MAX_POSTS)),
            })
        })
        .await
        .map_err(|_| Failure::Unavailable)?
    }

    fn majority_read(&self, name: &str) -> Collection<Document> {
        self.db.collection_with_options(
            name,
            CollectionOptions::builder()
                .read_concern(ReadConcern::majority())
                .selection_criteria(primary())
                .build(),
        )
    }

    /// The command can only be formed after current Inventory authorization.
    /// This lookup does not depend on the plan, destination, or Procurement.
    async fn replay(&self, command: Command) -> Result<Option<Saved>, Failure> {
        timeout(DB_LIMIT, async {
            self.majority_read(RECEIPTS)
                .find_one(doc! {"_id": receipt_key(command)})
                .max_time(DB_LIMIT)
                .await
                .map_err(|_| Failure::Unavailable)?
                .map(|saved| records::read_saved(command, &saved).map_err(Failure::Rejected))
                .transpose()
        })
        .await
        .map_err(|_| Failure::Unavailable)?
    }

    /// Read only an existing allocation for the exact authorized Sales and purchase tuple.
    async fn allocation(
        &self,
        sales: SalesLine,
        plan: Uuid,
        purchase: PurchaseLine,
    ) -> Result<Option<Allocation>, Failure> {
        timeout(DB_LIMIT, async {
            let Some(document) = self
                .majority_read(LINES)
                .find_one(doc! {
                    "purchaseKey": purchase_key(sales.scope, plan, purchase),
                    "reservationKey": sales_key(sales),
                })
                .max_time(DB_LIMIT)
                .await
                .map_err(|_| Failure::Unavailable)?
            else {
                return Ok(None);
            };
            let (_, allocation) = records::read_line(&document).map_err(Failure::Rejected)?;
            Ok(Some(allocation))
        })
        .await
        .map_err(|_| Failure::Unavailable)?
    }

    async fn post(
        &self,
        command: Command,
        issued: &IssuedLine<'_>,
        destination: &StockKey,
        actor_kind: ActorKind,
    ) -> Result<Post, Failure> {
        let store = self.clone();
        let destination = destination.clone();
        let IssuedLine {
            scope,
            actor,
            plan,
            purchase,
            item,
            unit,
            ordered,
        } = *issued;
        let item = item.clone();
        let unit = unit.to_owned();
        let deadline = Instant::now() + DB_LIMIT;
        bounded_post(&self.posting_slots, deadline, async move {
            let issued = IssuedLine {
                scope,
                actor,
                plan,
                purchase,
                item: &item,
                unit: &unit,
                ordered,
            };
            store
                .transact(command, &issued, &destination, actor_kind, deadline)
                .await
        })
        .await
    }

    async fn transact(
        &self,
        command: Command,
        issued: &IssuedLine<'_>,
        destination: &StockKey,
        actor_kind: ActorKind,
        deadline: Instant,
    ) -> Result<Post, Failure> {
        let mut session = self
            .db
            .client()
            .start_session()
            .await
            .map_err(|_| Failure::Unavailable)?;
        let message_id = Uuid::new_v4();
        for attempt in 0..MAX_ATTEMPTS {
            if Instant::now() >= deadline {
                return Err(Failure::Unavailable);
            }
            session
                .start_transaction()
                .read_concern(ReadConcern::snapshot())
                .selection_criteria(primary())
                .write_concern(majority_write())
                .max_commit_time(COMMIT_LIMIT)
                .await
                .map_err(|_| Failure::Unavailable)?;

            let result = self
                .apply(
                    &mut session,
                    command,
                    issued,
                    destination,
                    actor_kind,
                    message_id,
                )
                .await;
            match result {
                Ok(post) => {
                    if confirm_commit(&mut session).await? {
                        return Ok(post);
                    }
                }
                Err(step) => {
                    // No commit was sent. Abort before retrying a contended transaction.
                    session
                        .abort_transaction()
                        .await
                        .map_err(|_| Failure::Unavailable)?;
                    match step {
                        Step::Rejected(reject) => return Err(Failure::Rejected(reject)),
                        Step::Database(error) if transient(&error) || duplicate(&error) => (),
                        Step::Database(_) => return Err(Failure::Unavailable),
                    }
                }
            }
            retry_pause(attempt).await;
        }
        Err(Failure::Unavailable)
    }

    async fn apply(
        &self,
        session: &mut ClientSession,
        command: Command,
        issued: &IssuedLine<'_>,
        destination: &StockKey,
        actor_kind: ActorKind,
        message_id: Uuid,
    ) -> Result<Post, Step> {
        let receipts = self.db.collection::<Document>(RECEIPTS);
        if let Some(document) = receipts
            .find_one(doc! {"_id": receipt_key(command)})
            .session(&mut *session)
            .await?
        {
            return Ok(Post::Replay(records::read_saved(command, &document)?));
        }
        if issued.scope != command.sales.scope || issued.purchase != command.purchase {
            return Err(Reject::NotFound.into());
        }
        if issued.actor != command.actor || issued.plan.is_nil() || !issued.item.valid() {
            return Err(Reject::InvalidSource.into());
        }

        let saved_plan = self
            .db
            .collection::<Document>("material_plans")
            .find_one(doc! {"_id": sales_key(command.sales)})
            .session(&mut *session)
            .await?
            .ok_or(Reject::NotFound)?;
        let (plan, waiting_filter) =
            records::original(&saved_plan, command.sales, issued.plan, issued.item)?;
        let row = row_key(&plan.key);
        let purchase = purchase_key(command.sales.scope, plan.key.plan, command.purchase);
        let lines = self.db.collection::<Document>(LINES);
        let old = lines
            .find_one(doc! {"_id": &row})
            .session(&mut *session)
            .await?
            .map(|document| records::read_line(&document))
            .transpose()?;
        if old
            .as_ref()
            .is_some_and(|(line, _)| line.binding.row != plan)
        {
            return Err(Reject::InvalidState.into());
        }
        let exact = old
            .as_ref()
            .map(|(line, _)| line)
            .filter(|line| line.binding.purchase == command.purchase);
        if exact.is_none()
            && lines
                .find_one(doc! {"purchaseKey": &purchase})
                .projection(doc! {"_id": 1})
                .session(&mut *session)
                .await?
                .is_some()
        {
            return Err(Reject::PlanIneligible.into());
        }

        let reservations = self.db.collection::<Document>("reservations");
        let waiting = reservations
            .find_one(waiting_filter.clone())
            .session(&mut *session)
            .await?
            .is_some_and(|row| records::is_waiting(&row, &waiting_filter));
        let current = CurrentRow {
            key: plan.key.clone(),
            eligibility: if waiting {
                Eligibility::Waiting
            } else {
                Eligibility::Incompatible
            },
            purchase: old.as_ref().map(|(line, _)| line.binding.purchase),
            held: old
                .as_ref()
                .map_or(plan.original.held, |(_, allocation)| allocation.held),
        };
        let balances = self.db.collection::<Document>("balances");
        let stock = stock_key(&plan.key.stock);
        let stock_filter = doc! {"_id": &stock, "schemaVersion": SCHEMA, "available": {"$gte": 0}};
        let mapped = destination == &plan.key.stock
            && balances
                .find_one(stock_filter.clone())
                .session(&mut *session)
                .await?
                .is_some_and(|row| {
                    row.get_document("_id") == Ok(&stock)
                        && row.get_i32("schemaVersion") == Ok(SCHEMA)
                        && row
                            .get_i32("available")
                            .is_ok_and(|available| available >= 0)
                });
        let effect = post_new(
            command,
            &plan,
            &current,
            issued,
            exact,
            mapped.then_some(destination),
        )?;
        let saved = Saved {
            effect,
            posted_at: DateTime::now(),
            message_id,
        };
        let next = records::line_record(&saved.effect)?;
        let receipt = records::receipt_record(&saved, &next, fact::encode(&saved, actor_kind)?);

        // Real writes (not matched no-ops) guard against deletion/changes after
        // the snapshot. These tokens do not change demand or available stock.
        guard(&reservations, waiting_filter, message_id, session).await?;
        guard(&balances, stock_filter, message_id, session).await?;
        if let Some((_, previous)) = old {
            let changed = lines
                .update_one(
                    doc! {"_id": &row, "purchaseKey": &purchase, "allocation.sequence": previous.sequence},
                    doc! {"$set": {"allocation": next.get_document("allocation").map_err(|_| Reject::InvalidState)?}},
                )
                .session(&mut *session)
                .await?;
            if changed.modified_count != 1 {
                return Err(Reject::InvalidState.into());
            }
        } else {
            lines.insert_one(next).session(&mut *session).await?;
        }
        receipts.insert_one(receipt).session(&mut *session).await?;
        self.db
            .collection::<Document>(LEDGER)
            .insert_one(records::movement(&saved))
            .session(session)
            .await?;
        Ok(Post::Created(saved))
    }
}

async fn bounded_post<T: Send + 'static>(
    slots: &Arc<Semaphore>,
    deadline: Instant,
    operation: impl Future<Output = Result<T, Failure>> + Send + 'static,
) -> Result<T, Failure> {
    let slot = slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| Failure::Unavailable)?;
    // Timing out or dropping the caller does not cancel the Mongo future,
    // including an in-flight commit. The task retains its slot until it
    // finishes; a client with an uncertain outcome retries the same intent.
    let task = tokio::spawn(async move {
        let _slot = slot;
        operation.await
    });
    timeout_at(deadline, task)
        .await
        .map_err(|_| Failure::Unavailable)?
        .map_err(|_| Failure::Unavailable)?
}

async fn guard(
    collection: &Collection<Document>,
    filter: Document,
    message_id: Uuid,
    session: &mut ClientSession,
) -> Result<(), Step> {
    let result = collection
        .update_one(
            filter,
            doc! {"$set": {"purchaseReceiptGuard": message_id.to_string()}},
        )
        .session(session)
        .await?;
    if result.modified_count != 1 {
        return Err(Reject::InvalidState.into());
    }
    Ok(())
}

fn transient(error: &MongoError) -> bool {
    error.contains_label(TRANSIENT_TRANSACTION_ERROR)
}

fn duplicate(error: &MongoError) -> bool {
    matches!(&*error.kind, ErrorKind::Write(WriteFailure::WriteError(e)) if e.code == 11000)
}

async fn retry_pause(attempt: u32) {
    sleep(Duration::from_millis(10 << attempt)).await;
}

/// An unknown result may have committed; only resend commit on this session.
/// `false` permits a fresh attempt only for a definite transient abort.
async fn confirm_commit(session: &mut ClientSession) -> Result<bool, Failure> {
    let mut uncertain = false;
    for attempt in 0..MAX_ATTEMPTS {
        match session.commit_transaction().await {
            Ok(()) => return Ok(true),
            Err(error) if error.contains_label(UNKNOWN_TRANSACTION_COMMIT_RESULT) => {
                uncertain = true;
                retry_pause(attempt).await;
            }
            Err(error) if !uncertain && transient(&error) => return Ok(false),
            Err(_) => return Err(Failure::Unavailable),
        }
    }
    Err(Failure::Unavailable)
}
