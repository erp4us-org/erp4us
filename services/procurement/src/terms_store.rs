use crate::{
    Scope, purchase_store,
    terms::{Command, Decision, Failure, Saved, Terms},
};
use futures_util::future::BoxFuture;
use mongodb::{
    ClientSession, Collection, Database, IndexModel,
    bson::{Document, doc},
    error::{
        Error as MongoError, ErrorKind, TRANSIENT_TRANSACTION_ERROR,
        UNKNOWN_TRANSACTION_COMMIT_RESULT, WriteFailure,
    },
    options::{
        Acknowledgment, CollectionOptions, IndexOptions, ReadConcern, ReadPreference,
        SelectionCriteria, WriteConcern,
    },
};
use serde::Serialize;
use std::{io, time::Duration};
use tokio::time::{Instant, sleep_until};
use uuid::Uuid;

const REVISIONS: &str = "purchase_terms_revisions";
const GUARDS: &str = "proposal_write_guards";
pub(super) const DB_TIMEOUT: Duration = Duration::from_secs(3);
const RETRY_DELAY: Duration = Duration::from_millis(15);

#[derive(Clone)]
pub(super) struct Store {
    revisions: Collection<Saved>,
    guard: WriteGuard,
    purchases: purchase_store::Store,
}

pub(super) fn scoped_plan(scope: Scope, plan_id: Uuid) -> Document {
    doc! {
        "tenantId": scope.tenant_id.to_string(),
        "organizationId": scope.organization_id.to_string(),
        "planId": plan_id.to_string(),
    }
}

fn guard_key(scope: Scope, plan_id: Uuid) -> Document {
    doc! {"_id": scoped_plan(scope, plan_id)}
}

fn primary() -> SelectionCriteria {
    SelectionCriteria::ReadPreference(ReadPreference::Primary)
}

fn majority_write() -> WriteConcern {
    WriteConcern::builder()
        .w(Acknowledgment::Majority)
        .w_timeout(DB_TIMEOUT)
        .build()
}

pub(super) fn majority_collection() -> CollectionOptions {
    CollectionOptions::builder()
        .read_concern(ReadConcern::majority())
        .selection_criteria(primary())
        .write_concern(majority_write())
        .build()
}

pub(super) async fn ensure_collection(db: &Database, name: &str) -> Result<(), MongoError> {
    match db
        .create_collection(name)
        .write_concern(majority_write())
        .await
    {
        Err(error) if matches!(&*error.kind, ErrorKind::Command(err) if err.code == 48) => Ok(()),
        result => result,
    }
}

#[derive(Clone)]
pub(super) struct WriteGuard(Database);

pub(super) enum TransactionError<F> {
    Rule(F),
    Database(MongoError),
}

type TransactionFuture<'a, T, F> = BoxFuture<'a, Result<T, TransactionError<F>>>;

impl<F> From<MongoError> for TransactionError<F> {
    fn from(error: MongoError) -> Self {
        Self::Database(error)
    }
}

impl WriteGuard {
    async fn open(db: &Database) -> io::Result<Self> {
        // Initialize on upgrade as well as a fresh database; the guard has
        // only its built-in unique _id index.
        ensure_collection(db, GUARDS)
            .await
            .map_err(|_| io::Error::other("Cannot initialize Procurement proposal write guard"))?;
        Ok(Self(db.clone()))
    }

    pub(super) async fn write<T: Send, F: Copy + Send, C: Sync>(
        &self,
        target: (Scope, Uuid),
        deadline: Instant,
        unavailable: F,
        context: &C,
        apply: impl for<'a> Fn(&'a C, &'a mut ClientSession) -> TransactionFuture<'a, T, F> + Send,
    ) -> Result<T, F> {
        self.transact(
            Some(guard_key(target.0, target.1)),
            deadline,
            unavailable,
            context,
            apply,
        )
        .await
    }

    pub(super) async fn snapshot<T: Send, F: Copy + Send, C: Sync>(
        &self,
        deadline: Instant,
        unavailable: F,
        context: &C,
        read: impl for<'a> Fn(&'a C, &'a mut ClientSession) -> TransactionFuture<'a, T, F> + Send,
    ) -> Result<T, F> {
        self.transact(None, deadline, unavailable, context, read)
            .await
    }

    async fn transact<T: Send, F: Copy + Send, C: Sync>(
        &self,
        guard: Option<Document>,
        deadline: Instant,
        unavailable: F,
        context: &C,
        apply: impl for<'a> Fn(&'a C, &'a mut ClientSession) -> TransactionFuture<'a, T, F> + Send,
    ) -> Result<T, F> {
        let mut session = self
            .0
            .client()
            .start_session()
            .await
            .map_err(|_| unavailable)?;
        let mut delay = RETRY_DELAY;
        while Instant::now() < deadline {
            session
                .start_transaction()
                .read_concern(ReadConcern::snapshot())
                .selection_criteria(primary())
                .write_concern(majority_write())
                .max_commit_time(DB_TIMEOUT)
                .await
                .map_err(|_| unavailable)?;

            if let Some(key) = &guard {
                // This must be the FIRST operation. A real write (not a no-op)
                // conflicts with an edit/approval that changed the guard after
                // our snapshot. Only after it succeeds may we read latest terms.
                let result = self
                    .0
                    .collection::<Document>(GUARDS)
                    .update_one(key.clone(), doc! {"$inc": {"generation": 1_i64}})
                    .upsert(true)
                    .session(&mut session)
                    .await;
                if let Err(error) = result {
                    session.abort_transaction().await.map_err(|_| unavailable)?;
                    // Concurrent first use of this _id can race the upsert
                    // without the driver attaching a transient label.
                    if transient(&error) || duplicate_key(&error) {
                        retry(deadline, &mut delay).await;
                        continue;
                    }
                    return Err(unavailable);
                }
            }

            if Instant::now() >= deadline {
                session.abort_transaction().await.map_err(|_| unavailable)?;
                return Err(unavailable);
            }
            let value = match apply(context, &mut session).await {
                Ok(value) => value,
                Err(error) => {
                    session.abort_transaction().await.map_err(|_| unavailable)?;
                    match error {
                        TransactionError::Rule(failure) => return Err(failure),
                        TransactionError::Database(error) if transient(&error) => {
                            retry(deadline, &mut delay).await;
                            continue;
                        }
                        TransactionError::Database(_) => return Err(unavailable),
                    }
                }
            };
            if Instant::now() >= deadline {
                session.abort_transaction().await.map_err(|_| unavailable)?;
                return Err(unavailable);
            }

            // An uncertain commit may already have applied. Only retry the
            // commit on this session; never report success without confirmation
            // or run the operation again after an unresolved commit.
            let mut commit_uncertain = false;
            loop {
                match session.commit_transaction().await {
                    Ok(()) => return Ok(value),
                    Err(error) if error.contains_label(UNKNOWN_TRANSACTION_COMMIT_RESULT) => {
                        commit_uncertain = true;
                        retry(deadline, &mut delay).await;
                        if Instant::now() >= deadline {
                            return Err(unavailable);
                        }
                    }
                    Err(error) if !commit_uncertain && transient(&error) => {
                        retry(deadline, &mut delay).await;
                        break;
                    }
                    Err(_) => return Err(unavailable),
                }
            }
        }
        Err(unavailable)
    }
}

fn transient(error: &MongoError) -> bool {
    error.contains_label(TRANSIENT_TRANSACTION_ERROR)
}

fn duplicate_key(error: &MongoError) -> bool {
    matches!(&*error.kind, ErrorKind::Write(WriteFailure::WriteError(err)) if err.code == 11000)
}

async fn retry(deadline: Instant, delay: &mut Duration) {
    sleep_until(deadline.min(Instant::now() + *delay)).await;
    *delay = (*delay * 2).min(Duration::from_millis(150));
}

pub(super) enum Save {
    Created(Terms),
    Replay(Terms),
}

fn scope_key(scope: Scope, plan_id: Uuid) -> Document {
    doc! {
        "tenantId": scope.tenant_id.to_string(),
        "organizationId": scope.organization_id.to_string(),
        "terms.plan_id": plan_id.to_string(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Record<'a> {
    #[serde(with = "uuid::serde::hyphenated")]
    tenant_id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    organization_id: Uuid,
    #[serde(flatten)]
    value: &'a Saved,
}

impl Store {
    pub(super) async fn open(db: &Database) -> io::Result<Self> {
        let guard = WriteGuard::open(db).await?;
        let collection = db.collection_with_options(REVISIONS, majority_collection());
        let unique = |keys| {
            IndexModel::builder()
                .keys(keys)
                .options(IndexOptions::builder().unique(true).build())
                .build()
        };
        collection
            .create_indexes([
                unique(doc! {"tenantId": 1, "organizationId": 1, "terms.plan_id": 1, "terms.revision": -1}),
                unique(doc! {"tenantId": 1, "organizationId": 1, "terms.plan_id": 1, "commandId": 1}),
            ])
            .max_time(DB_TIMEOUT)
            .await
            .map_err(|_| io::Error::other("Cannot initialize Procurement terms indexes"))?;
        Ok(Self {
            revisions: collection,
            guard,
            purchases: purchase_store::Store::open(db).await?,
        })
    }

    pub(super) fn guard(&self) -> &WriteGuard {
        &self.guard
    }

    pub(super) fn purchases(&self) -> &purchase_store::Store {
        &self.purchases
    }

    pub(super) async fn latest(
        &self,
        scope: Scope,
        plan_id: Uuid,
    ) -> Result<Option<Terms>, Failure> {
        self.revisions
            .find_one(scope_key(scope, plan_id))
            .max_time(DB_TIMEOUT)
            .sort(doc! {"terms.revision": -1})
            .await
            .map(|stored| stored.map(|saved| saved.terms))
            .map_err(|_| Failure::Unavailable)
    }

    pub(super) async fn latest_in(
        &self,
        scope: Scope,
        plan_id: Uuid,
        session: &mut ClientSession,
    ) -> Result<Option<Saved>, MongoError> {
        self.revisions
            .find_one(scope_key(scope, plan_id))
            .max_time(DB_TIMEOUT)
            .sort(doc! {"terms.revision": -1})
            .session(session)
            .await
    }

    pub(super) async fn decide(
        &self,
        scope: Scope,
        plan_id: Uuid,
        command: &Command,
    ) -> Result<Decision, Failure> {
        // Read in this order so a same-command race reaching the latest query
        // also gets a chance to be recognized by the command lookup.
        let latest = self.latest(scope, plan_id).await?;
        let mut key = scope_key(scope, plan_id);
        key.insert("commandId", command.command_id.to_string());
        let saved = self
            .revisions
            .find_one(key)
            .max_time(DB_TIMEOUT)
            .await
            .map_err(|_| Failure::Unavailable)?;
        let decision = command.decide(latest.map_or(0, |terms| terms.revision), saved)?;
        // Avoid new supplier lookups for issued plans. The actual freeze is
        // checked again in append after taking the same guard as issuance.
        if matches!(decision, Decision::Create(_))
            && self
                .purchases
                .exists(scope, plan_id, None)
                .await
                .map_err(|_| Failure::Unavailable)?
        {
            return Err(Failure::Conflict);
        }
        Ok(decision)
    }

    pub(super) async fn append(
        &self,
        scope: Scope,
        command: &Command,
        saved: Saved,
        deadline: Instant,
    ) -> Result<Save, Failure> {
        let plan_id = saved.terms.plan_id;
        let document = mongodb::bson::to_document(&Record {
            tenant_id: scope.tenant_id,
            organization_id: scope.organization_id,
            value: &saved,
        })
        .map_err(|_| Failure::Unavailable)?;
        let mut key = scope_key(scope, plan_id);
        key.insert("commandId", command.command_id.to_string());
        let replay = self
            .guard
            .write(
                (scope, plan_id),
                deadline,
                Failure::Unavailable,
                &(
                    self,
                    scope,
                    plan_id,
                    command,
                    saved.terms.revision,
                    key,
                    document,
                ),
                |(store, scope, plan_id, command, proposed_revision, key, document), session| {
                    Box::pin(async move {
                        let latest = store.latest_in(*scope, *plan_id, session).await?;
                        let receipt = store
                            .revisions
                            .find_one(key.clone())
                            .max_time(DB_TIMEOUT)
                            .session(&mut *session)
                            .await?;
                        match command
                            .decide(latest.map_or(0, |r| r.terms.revision), receipt)
                            .map_err(TransactionError::Rule)?
                        {
                            Decision::Replay(terms) => Ok(Some(terms)),
                            Decision::Create(revision) if revision == *proposed_revision => {
                                if store
                                    .purchases
                                    .exists(*scope, *plan_id, Some(&mut *session))
                                    .await?
                                {
                                    return Err(TransactionError::Rule(Failure::Conflict));
                                }
                                store
                                    .revisions
                                    .clone_with_type::<Document>()
                                    .insert_one(document.clone())
                                    .session(&mut *session)
                                    .await?;
                                Ok(None)
                            }
                            Decision::Create(_) => Err(TransactionError::Rule(Failure::Conflict)),
                        }
                    })
                },
            )
            .await?;
        Ok(match replay {
            Some(terms) => Save::Replay(terms),
            None => Save::Created(saved.terms),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terms::tests::{command, fixture};
    use mongodb::bson;
    use std::collections::HashMap;

    #[test]
    fn guard_has_one_unique_id_for_each_tenant_organization_and_plan() {
        let (scope, draft, _, _) = fixture();
        let plan_id = draft.plan.plan_id;
        let key = guard_key(scope, plan_id);
        assert_eq!(
            key,
            doc! {"_id": {
                "tenantId": scope.tenant_id.to_string(),
                "organizationId": scope.organization_id.to_string(),
                "planId": plan_id.to_string(),
            }}
        );
        assert_ne!(key, guard_key(scope, Uuid::new_v4()));
        for foreign in [
            Scope {
                tenant_id: Uuid::new_v4(),
                ..scope
            },
            Scope {
                organization_id: Uuid::new_v4(),
                ..scope
            },
        ] {
            assert_ne!(key, guard_key(foreign, plan_id));
        }
    }

    #[test]
    fn bson_keeps_scoped_index_fields_audit_and_original_response() {
        let (scope, draft, body, supplier) = fixture();
        let command = command(&body).unwrap();
        let actor = Uuid::new_v4();
        let saved = command
            .bind(&draft)
            .unwrap()
            .snapshot(1, &HashMap::from([(supplier.id, supplier)]), actor)
            .unwrap();
        let doc = bson::to_document(&Record {
            tenant_id: scope.tenant_id,
            organization_id: scope.organization_id,
            value: &saved,
        })
        .unwrap();
        let plan_id = draft.plan.plan_id;
        assert_eq!(
            scope_key(scope, plan_id),
            doc! {
                "tenantId": scope.tenant_id.to_string(),
                "organizationId": scope.organization_id.to_string(),
                "terms.plan_id": plan_id.to_string(),
            }
        );
        for foreign in [
            Scope {
                tenant_id: Uuid::new_v4(),
                ..scope
            },
            Scope {
                organization_id: Uuid::new_v4(),
                ..scope
            },
        ] {
            assert_ne!(scope_key(scope, plan_id), scope_key(foreign, plan_id));
        }
        assert_ne!(scope_key(scope, plan_id), scope_key(scope, Uuid::new_v4()));
        assert_eq!(
            doc.get_str("tenantId").unwrap(),
            scope.tenant_id.to_string()
        );
        assert_eq!(
            doc.get_str("organizationId").unwrap(),
            scope.organization_id.to_string()
        );
        assert_eq!(
            doc.get_str("commandId").unwrap(),
            command.command_id.to_string()
        );
        assert_eq!(doc.get_str("actorId").unwrap(), actor.to_string());
        assert_eq!(doc.get_i32("expectedRevision"), Ok(0));
        assert_eq!(doc.get_i32("schemaVersion"), Ok(1));
        assert!(doc.get_datetime("savedAt").is_ok());
        let terms = doc.get_document("terms").unwrap();
        assert_eq!(terms.get_str("plan_id").unwrap(), plan_id.to_string());
        assert_eq!(terms.get_str("status"), Ok("draft"));
        assert_eq!(terms.get_i32("revision"), Ok(1));
        assert_eq!(terms.get_str("total"), Ok("162.42"));
        let line = terms.get_array("lines").unwrap()[0].as_document().unwrap();
        assert_eq!(line.get_str("pieces"), Ok("6"));
        assert_eq!(line.get_str("unit_price"), Ok("10.01"));
        assert_eq!(line.get_str("line_total"), Ok("60.06"));
        assert_eq!(line.get_document("supplier").unwrap().len(), 3);
        assert_eq!(bson::from_document::<Saved>(doc).unwrap(), saved);
    }
}
