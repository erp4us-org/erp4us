//! Test-only Procurement persistence proof. The immutable inbox doubles as the
//! staged queue: unique scoped message and receipt IDs and a unique scoped
//! purchase tuple/sequence. No per-line history is loaded into the summary.
//!
//! The later caller must authorize the Inventory publication principal, run
//! the independent wire decoder and supply the exact authorized, immutable
//! Procurement line. Staged/replay outcomes are durable only after majority
//! commit. Other outcomes are not accepted deliveries: the first linked
//! conflict marker is not a per-delivery quarantine or terminal ACK policy.
//! Known gaps/integrity issues are unknown, not inferred zero. The caller must
//! keep the store operation alive through commit resolution; a broker lease
//! or HTTP deadline must not cancel the underlying transaction.

use super::*;
use crate::terms_store::{DB_TIMEOUT, ensure_collection, majority_collection};
use mongodb::{
    ClientSession, Database, IndexModel,
    bson::{self, DateTime, Document, doc},
    error::{
        Error as MongoError, ErrorKind, TRANSIENT_TRANSACTION_ERROR,
        UNKNOWN_TRANSACTION_COMMIT_RESULT, WriteFailure,
    },
    options::{IndexOptions, ReadConcern},
};
use tokio::time::{Instant, sleep, timeout};

mod records;
mod tests;

use records::{
    Inbox, LineId, Prepared, Quarantine, Reason, Summary, message_filter, receipt_filter,
    sequence_filter,
};

const INBOX: &str = "purchase_receipt_inbox";
const SUMMARIES: &str = "purchase_receipt_observations";
const MAX_DRAIN: usize = 4;
const MAX_ATTEMPTS: u32 = 5;

/// Construct from the verified scope and Procurement's actual issued record,
/// not from the Inventory fact or a client-supplied snapshot.
#[derive(Clone, Copy)]
struct IssuedLine<'a> {
    key: LineKey<'a>,
    ordered: u32,
}

impl<'a> IssuedLine<'a> {
    fn new(scope: Scope, key: LineKey<'a>, ordered: &str) -> Result<Self, Failure> {
        if (scope.tenant_id, scope.organization_id) != (key.tenant_id, key.organization_id) {
            return Err(Failure::InvalidLine);
        }
        let projection = Projection::new(key, ordered).map_err(|_| Failure::InvalidLine)?;
        Ok(Self {
            key,
            ordered: projection.ordered,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Failure {
    InvalidLine,
    InvalidFact,
    /// Includes uncertain commit. Redelivery must preserve the original IDs.
    Unavailable,
}

impl From<Failure> for MongoError {
    fn from(failure: Failure) -> Self {
        Self::custom(failure)
    }
}

#[derive(Debug, Eq, PartialEq)]
struct View {
    state: Observation,
    projected_at: Option<DateTime>,
}

struct Store(Database);

impl Store {
    async fn open(db: &Database) -> Result<Self, Failure> {
        timeout(DB_TIMEOUT, async {
            for name in [INBOX, SUMMARIES] {
                ensure_collection(db, name).await?;
            }
            let unique = |name: &str, keys| {
                IndexModel::builder()
                    .keys(keys)
                    .options(
                        IndexOptions::builder()
                            .name(name.to_owned())
                            .unique(true)
                            .build(),
                    )
                    .build()
            };
            let scope = doc! {"_id.tenantId": 1, "_id.organizationId": 1};
            let mut receipt = scope.clone();
            receipt.insert("receiptId", 1);
            let indexes = [
                unique("scoped_receipt", receipt),
                unique("scoped_line_sequence", doc! {"line": 1, "sequence": 1}),
            ];
            db.collection_with_options::<Inbox>(INBOX, majority_collection())
                .create_indexes(indexes)
                .await?;
            let mut pending = scope;
            pending.insert("pending", 1);
            db.collection_with_options::<Summary>(SUMMARIES, majority_collection())
                .create_index(IndexModel::builder().keys(pending).build())
                .await?;
            Ok::<_, MongoError>(Self(db.clone()))
        })
        .await
        .map_err(|_| Failure::Unavailable)?
        .map_err(|_| Failure::Unavailable)
    }

    async fn read(&self, issued: IssuedLine<'_>) -> Result<View, Failure> {
        let collection = self
            .0
            .collection_with_options::<Summary>(SUMMARIES, majority_collection());
        let summary = timeout(
            DB_TIMEOUT,
            collection
                .find_one(doc! {"_id": LineId::from(issued.key).key()})
                .max_time(DB_TIMEOUT)
                .into_future(),
        )
        .await
        .map_err(|_| Failure::Unavailable)?
        .map_err(|_| Failure::Unavailable)?
        .unwrap_or_else(|| Summary::initial(issued));
        let state = summary.projection(issued)?.observation();
        let projected_at = if state == Observation::Unknown {
            None
        } else {
            summary.projected_at
        };
        Ok(View {
            state,
            projected_at,
        })
    }

    async fn record(
        &self,
        issued: IssuedLine<'_>,
        publication_scope: Scope,
        event: &Envelope<ReceiptPosted>,
    ) -> Result<Admission, Failure> {
        let prepared = Prepared::decoded(publication_scope, event)?;
        if prepared.fact()?.line != issued.key {
            // A foreign or unmatched tuple must not invalidate a real line.
            return Ok(Admission::NotForLine);
        }
        self.transaction(async |session| {
            self.apply(issued, publication_scope, &prepared, session)
                .await
        })
        .await
    }

    /// Bounded maintenance for already staged facts, not a recovery/replay API.
    /// A future worker may discover candidates with scoped `pending: true`;
    /// a known gap is not runnable until a verified missing fact is staged.
    async fn continue_pending(&self, issued: IssuedLine<'_>) -> Result<(), Failure> {
        self.transaction(async |session| {
            let Some(mut summary) = self.get_summary(issued.key.into(), &mut *session).await?
            else {
                return Ok(());
            };
            let mut projection = summary.projection(issued)?;
            if summary.pending {
                self.drain(&mut summary, &mut projection, session).await?;
                self.save_summary(summary, true, session).await?;
            }
            Ok(())
        })
        .await
    }

    async fn get_summary(
        &self,
        line: LineId,
        session: &mut ClientSession,
    ) -> mongodb::error::Result<Option<Summary>> {
        self.0
            .collection::<Summary>(SUMMARIES)
            .find_one(doc! {"_id": line.key()})
            .session(session)
            .await
    }

    async fn save_summary(
        &self,
        summary: Summary,
        exists: bool,
        session: &mut ClientSession,
    ) -> mongodb::error::Result<()> {
        let collection = self.0.collection::<Summary>(SUMMARIES);
        if exists {
            // Mutating the same summary document serializes concurrent writes.
            let result = collection
                .replace_one(doc! {"_id": summary.line.key()}, summary)
                .session(session)
                .await?;
            if result.matched_count != 1 {
                return Err(Failure::Unavailable.into());
            }
        } else {
            collection.insert_one(summary).session(session).await?;
        }
        Ok(())
    }

    async fn get_fact(
        &self,
        filter: Document,
        session: &mut ClientSession,
    ) -> mongodb::error::Result<Option<Prepared>> {
        Ok(self
            .0
            .collection::<Inbox>(INBOX)
            .find_one(filter)
            .session(session)
            .await?
            .map(Prepared::stored)
            .transpose()?)
    }

    async fn apply(
        &self,
        issued: IssuedLine<'_>,
        scope: Scope,
        prepared: &Prepared,
        session: &mut ClientSession,
    ) -> mongodb::error::Result<Admission> {
        let fact = prepared.fact()?;
        let line_id = LineId::from(issued.key);
        let summary = self.get_summary(line_id, session).await?;
        let exists = summary.is_some();
        let mut summary = summary.unwrap_or_else(|| Summary::initial(issued));
        let mut projection = summary.projection(issued)?;
        let message = self
            .get_fact(message_filter(scope, fact.id.message_id), session)
            .await?;
        let receipt = self
            .get_fact(receipt_filter(scope, fact.id.receipt_id), session)
            .await?;
        let sequence = self
            .get_fact(sequence_filter(line_id, fact.id.sequence), session)
            .await?;
        let existing = Existing {
            message: message.as_ref().map(Prepared::fact).transpose()?,
            receipt: receipt.as_ref().map(Prepared::fact).transpose()?,
            sequence: sequence.as_ref().map(Prepared::fact).transpose()?,
        };
        let in_scope = |item: VerifiedReceipt<'_>| {
            item.line.tenant_id == scope.tenant_id
                && item.line.organization_id == scope.organization_id
        };
        if existing
            .message
            .is_some_and(|old| !in_scope(old) || old.id.message_id != fact.id.message_id)
            || existing
                .receipt
                .is_some_and(|old| !in_scope(old) || old.id.receipt_id != fact.id.receipt_id)
            || existing
                .sequence
                .is_some_and(|old| old.line != issued.key || old.id.sequence != fact.id.sequence)
        {
            return Err(Failure::Unavailable.into());
        }
        let result = projection.stage(fact, existing);
        match &result {
            Admission::Staged => {
                self.0
                    .collection::<Inbox>(INBOX)
                    .insert_one(&prepared.inbox)
                    .session(&mut *session)
                    .await?;
                self.drain(&mut summary, &mut projection, session).await?;
                self.save_summary(summary, exists, session).await?;
            }
            Admission::Conflict(conflict) => {
                let reason = match conflict {
                    Conflict::Message => Reason::Message,
                    Conflict::Receipt => Reason::Receipt,
                    Conflict::Sequence => Reason::Sequence,
                    Conflict::OverOrdered => Reason::OverOrdered,
                };
                let marker = Quarantine::new(reason, fact.id);
                summary.block(marker.clone());
                // Follow only the scoped, already verified immutable inbox
                // link, and only if its saved summary pins the same tuple.
                let mut blocked = None;
                for linked in [message, receipt].into_iter().flatten() {
                    if linked.inbox.line == line_id || blocked == Some(linked.inbox.line) {
                        continue;
                    }
                    let mut related = self
                        .get_summary(linked.inbox.line, session)
                        .await?
                        .ok_or(Failure::Unavailable)?;
                    related.check_link(&linked.inbox)?;
                    related.block(marker.clone());
                    blocked = Some(linked.inbox.line);
                    self.save_summary(related, true, session).await?;
                }
                self.save_summary(summary, exists, session).await?;
            }
            Admission::Replay if !exists => return Err(Failure::Unavailable.into()),
            Admission::Replay | Admission::Blocked | Admission::NotForLine => {}
        }
        Ok(result)
    }

    async fn drain(
        &self,
        summary: &mut Summary,
        projection: &mut Projection<'_>,
        session: &mut ClientSession,
    ) -> mongodb::error::Result<()> {
        let mut pending = false;
        for index in 0..MAX_DRAIN {
            let Some(sequence) = projection.next_to_apply() else {
                break;
            };
            let Some(staged) = self
                .get_fact(sequence_filter(summary.line, sequence), session)
                .await?
            else {
                if index == 0 && summary.pending {
                    // Previously durable runnable work cannot silently vanish.
                    return Err(Failure::Unavailable.into());
                }
                break;
            };
            let fact = staged.fact()?;
            match projection.apply_next(fact) {
                Ok(()) => {}
                Err(ApplyFailure::InconsistentStage) => {
                    summary.block(Quarantine::new(Reason::InconsistentStage, fact.id));
                    break;
                }
                Err(_) => return Err(Failure::Unavailable.into()),
            }
            if index + 1 == MAX_DRAIN
                && let Some(sequence) = projection.next_to_apply()
            {
                pending = self
                    .0
                    .collection::<Document>(INBOX)
                    .find_one(sequence_filter(summary.line, sequence))
                    .projection(doc! {"_id": 1})
                    .session(&mut *session)
                    .await?
                    .is_some();
            }
        }
        summary.save_prefix(projection, pending);
        Ok(())
    }

    async fn transaction<T>(
        &self,
        apply: impl for<'a> AsyncFn(&'a mut ClientSession) -> mongodb::error::Result<T>,
    ) -> Result<T, Failure> {
        let deadline = Instant::now() + DB_TIMEOUT;
        let mut session = self
            .0
            .client()
            .start_session()
            .await
            .map_err(|_| Failure::Unavailable)?;
        for attempt in 0..MAX_ATTEMPTS {
            // Bound fresh attempts, not the lifetime of an in-flight commit. In
            // particular, never replace the session while its result is uncertain.
            if Instant::now() >= deadline {
                break;
            }
            let opts = majority_collection();
            session
                .start_transaction()
                .read_concern(ReadConcern::snapshot())
                .selection_criteria(opts.selection_criteria.ok_or(Failure::Unavailable)?)
                .write_concern(opts.write_concern.ok_or(Failure::Unavailable)?)
                .max_commit_time(DB_TIMEOUT)
                .await
                .map_err(|_| Failure::Unavailable)?;
            match apply(&mut session).await {
                Ok(value) => {
                    if confirm_commit(&mut session).await? {
                        return Ok(value);
                    }
                }
                Err(error) => {
                    session
                        .abort_transaction()
                        .await
                        .map_err(|_| Failure::Unavailable)?;
                    // An insert race can be unlabeled (first summary or
                    // independently unique inbox IDs across two lines).
                    if !transient(&error) && !duplicate(&error) {
                        return Err(error
                            .get_custom::<Failure>()
                            .copied()
                            .unwrap_or(Failure::Unavailable));
                    }
                }
            }
            retry_pause(attempt).await;
        }
        Err(Failure::Unavailable)
    }
}

fn transient(error: &MongoError) -> bool {
    error.contains_label(TRANSIENT_TRANSACTION_ERROR)
}

fn duplicate(error: &MongoError) -> bool {
    matches!(&*error.kind, ErrorKind::Write(WriteFailure::WriteError(error)) if error.code == 11000)
}

async fn retry_pause(attempt: u32) {
    sleep(std::time::Duration::from_millis(10 << attempt)).await;
}

async fn confirm_commit(session: &mut ClientSession) -> Result<bool, Failure> {
    let mut uncertain = false;
    for attempt in 0..MAX_ATTEMPTS {
        match session.commit_transaction().await {
            Ok(()) => return Ok(true),
            Err(error) if error.contains_label(UNKNOWN_TRANSACTION_COMMIT_RESULT) => {
                // An unknown outcome only permits retrying commit, not the body.
                uncertain = true;
                retry_pause(attempt).await;
            }
            Err(error) if !uncertain && transient(&error) => return Ok(false),
            Err(_) => return Err(Failure::Unavailable),
        }
    }
    Err(Failure::Unavailable)
}
