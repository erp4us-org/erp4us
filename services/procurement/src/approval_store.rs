use crate::{
    Scope,
    budget_approvals::{Approval, Command, Failure, Record, Review},
    budget_policy::{MAX_STAGES, Policy},
    terms_store::{self, DB_TIMEOUT, TransactionError, majority_collection, scoped_plan},
};
use mongodb::{
    ClientSession, Collection, Database, IndexModel,
    bson::{self, Document, doc},
    error::Error as MongoError,
    options::IndexOptions,
};
use std::io;
use tokio::time::Instant;
use uuid::Uuid;

const DECISIONS: &str = "purchase_budget_approval_decisions";

#[derive(Clone)]
pub(super) struct Store(Collection<Record>);

pub(super) enum Saved {
    Created(Approval),
    Replay(Approval),
}

fn command_key(scope: Scope, plan_id: Uuid, command_id: Uuid) -> Document {
    let mut key = scoped_plan(scope, plan_id);
    key.insert("intent.command_id", command_id.to_string());
    key
}

fn version_key(scope: Scope, plan_id: Uuid, version: (i32, Uuid, i32)) -> Document {
    let (terms_revision, policy_id, policy_revision) = version;
    let mut key = scoped_plan(scope, plan_id);
    key.insert("intent.terms_revision", terms_revision);
    key.insert("intent.policy_id", policy_id.to_string());
    key.insert("intent.policy_revision", policy_revision);
    key
}

fn document(scope: Scope, plan_id: Uuid, record: &Record) -> Result<Document, Failure> {
    let mut entry = scoped_plan(scope, plan_id);
    entry.extend(bson::to_document(record).map_err(|_| Failure::Unavailable)?);
    Ok(entry)
}

impl Store {
    pub(super) async fn open(db: &Database) -> io::Result<Self> {
        let collection = db.collection_with_options(DECISIONS, majority_collection());
        let unique = |keys| {
            IndexModel::builder()
                .keys(keys)
                .options(IndexOptions::builder().unique(true).build())
                .build()
        };
        collection
            .create_indexes([
                unique(doc! {
                    "tenantId": 1, "organizationId": 1, "planId": 1, "intent.command_id": 1,
                }),
                unique(doc! {
                    "tenantId": 1, "organizationId": 1, "planId": 1,
                    "intent.terms_revision": 1, "intent.policy_id": 1, "intent.policy_revision": 1,
                    "stageIndex": 1,
                }),
            ])
            .max_time(DB_TIMEOUT)
            .await
            .map_err(|_| io::Error::other("Cannot initialize Procurement approval indexes"))?;
        Ok(Self(collection))
    }

    pub(super) async fn decisions(
        &self,
        scope: Scope,
        plan_id: Uuid,
        version: (i32, Uuid, i32),
        session: &mut ClientSession,
    ) -> Result<Vec<Record>, MongoError> {
        let mut cursor = self
            .0
            .find(version_key(scope, plan_id, version))
            .sort(doc! {"stageIndex": 1})
            .max_time(DB_TIMEOUT)
            // Detect unexpected extras without reading an unbounded history.
            .limit(MAX_STAGES as i64 + 1)
            .session(&mut *session)
            .await?;
        let mut decisions = Vec::new();
        while cursor.advance(&mut *session).await? {
            decisions.push(cursor.deserialize_current()?);
        }
        Ok(decisions)
    }

    pub(super) async fn get(
        &self,
        terms: &terms_store::Store,
        policy: Option<&Policy>,
        scope: Scope,
        plan_id: Uuid,
        deadline: Instant,
    ) -> Result<Approval, Failure> {
        terms
            .guard()
            .snapshot(
                deadline,
                Failure::Unavailable,
                &(self, terms, policy, scope, plan_id),
                |&(store, terms, policy, scope, plan_id), session| {
                    Box::pin(async move {
                        let saved = terms
                            .latest_in(scope, plan_id, session)
                            .await?
                            .ok_or(TransactionError::Rule(Failure::NotFound))?;
                        let review =
                            Review::new(scope, &saved, policy).map_err(TransactionError::Rule)?;
                        let decisions = store
                            .decisions(scope, plan_id, review.version(), session)
                            .await?;
                        review.result(&decisions).map_err(TransactionError::Rule)
                    })
                },
            )
            .await
    }

    pub(super) async fn append(
        &self,
        terms: &terms_store::Store,
        policy: Option<&Policy>,
        identity: erp4us_identity::Scope,
        plan_id: Uuid,
        command: &Command,
        deadline: Instant,
    ) -> Result<Saved, Failure> {
        let scope = identity.into();
        let actor_id = identity.actor;
        terms
            .guard()
            .write(
                (scope, plan_id),
                deadline,
                Failure::Unavailable,
                &(self, terms, policy, scope, plan_id, command, actor_id),
                |&(store, terms, policy, scope, plan_id, command, actor_id), session| {
                    Box::pin(async move {
                        let saved = terms
                            .latest_in(scope, plan_id, session)
                            .await?
                            .ok_or(TransactionError::Rule(Failure::NotFound))?;
                        let review =
                            Review::new(scope, &saved, policy).map_err(TransactionError::Rule)?;
                        if let Some(receipt) = store
                            .0
                            .find_one(command_key(scope, plan_id, command.intent.command_id))
                            .max_time(DB_TIMEOUT)
                            .session(&mut *session)
                            .await?
                        {
                            let original = receipt
                                .replay(command, actor_id, &review)
                                .map_err(TransactionError::Rule)?;
                            return Ok(Saved::Replay(original));
                        }
                        review
                            .check_version(&command.intent)
                            .map_err(TransactionError::Rule)?;
                        let decisions = store
                            .decisions(scope, plan_id, review.version(), session)
                            .await?;
                        let record = review
                            .decide(&decisions, command, actor_id)
                            .map_err(TransactionError::Rule)?;
                        // This immutable decision doubles as the scoped command receipt;
                        // the business effect and original result cannot commit separately.
                        let entry =
                            document(scope, plan_id, &record).map_err(TransactionError::Rule)?;
                        store
                            .0
                            .clone_with_type::<Document>()
                            .insert_one(entry)
                            .session(&mut *session)
                            .await?;
                        Ok(Saved::Created(record.original_result))
                    })
                },
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget_approvals::tests::{APPROVER, policy, stage, terms_fixture};

    #[test]
    fn immutable_bson_record_carries_scoped_indexes_intent_actor_and_replay_result() {
        let (scope, terms) = terms_fixture(1);
        let plan_id = terms.terms.plan_id;
        let policy = policy();
        let review = Review::new(scope, &terms, Some(&policy)).unwrap();
        let command = stage(&review, "department");
        let record = review.decide(&[], &command, APPROVER).unwrap();
        let doc = document(scope, plan_id, &record).unwrap();
        let version = review.version();
        let command_selector = command_key(scope, plan_id, command.intent.command_id);
        let version_selector = version_key(scope, plan_id, version);
        let intent = doc.get_document("intent").unwrap();
        assert_eq!(doc.get_i32("stageIndex"), Ok(1));
        assert_eq!(doc.get_i32("schemaVersion"), Ok(1));
        assert!(doc.get_datetime("decidedAt").is_ok());
        assert_eq!(doc.get_str("actorId").unwrap(), APPROVER.to_string());
        assert_eq!(intent.get_str("stage_key"), Ok("department"));
        assert_eq!(
            intent.get("command_id"),
            command_selector.get("intent.command_id")
        );
        for field in ["terms_revision", "policy_id", "policy_revision"] {
            assert_eq!(
                intent.get(field),
                version_selector.get(format!("intent.{field}"))
            );
        }
        for (field, value) in scoped_plan(scope, plan_id) {
            assert_eq!(doc.get(&field), Some(&value));
            assert_eq!(command_selector.get(&field), Some(&value));
            assert_eq!(version_selector.get(&field), Some(&value));
        }
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
            assert_ne!(
                command_selector,
                command_key(foreign, plan_id, command.intent.command_id)
            );
            assert_ne!(version_selector, version_key(foreign, plan_id, version));
        }
        assert_ne!(
            version_selector,
            version_key(scope, Uuid::new_v4(), version)
        );
        let restored: Record = bson::from_document(doc).unwrap();
        assert_eq!(restored, record);
        assert_eq!(
            restored.replay(&command, APPROVER, &review),
            Ok(record.original_result)
        );
    }
}
