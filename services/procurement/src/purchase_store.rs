use crate::{
    Scope, approval_store, budget_approvals, budget_policy, drafts,
    purchase_batch::{Batch, Intent, Issuance, IssueFailure},
    terms,
    terms_store::{self, DB_TIMEOUT, TransactionError, majority_collection, scoped_plan},
};
use mongodb::{
    ClientSession, Collection, Database,
    bson::{self, Document, doc},
    error::Error as MongoError,
};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use uuid::Uuid;

const BATCHES: &str = "issued_purchase_batches";

#[derive(Clone)]
pub(super) struct Store(Collection<Record>);

pub(super) struct Issue<'a> {
    pub drafts: &'a drafts::Store,
    pub terms: &'a terms_store::Store,
    pub approvals: &'a approval_store::Store,
    pub policy: Option<&'a budget_policy::Policy>,
    pub identity: erp4us_identity::Scope,
    pub plan_id: Uuid,
    pub intent: Intent,
}

pub(super) enum Saved {
    Created(Batch),
    Replay(Batch),
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    schema_version: i32,
    batch: Batch,
    approved_terms: terms::Saved,
    approval_decisions: Vec<budget_approvals::Record>,
}

fn key(scope: Scope, plan_id: Uuid) -> Document {
    doc! {"_id": scoped_plan(scope, plan_id)}
}

impl Record {
    fn checked_batch(self, plan_id: Uuid) -> Result<Batch, IssueFailure> {
        if self.schema_version != 1 || self.batch.plan_id() != plan_id {
            return Err(IssueFailure::Unavailable);
        }
        Ok(self.batch)
    }

    fn to_document(&self, scope: Scope) -> Result<Document, IssueFailure> {
        let mut document = key(scope, self.batch.plan_id());
        document.extend(bson::to_document(self).map_err(|_| IssueFailure::Unavailable)?);
        Ok(document)
    }
}

impl Store {
    pub(super) async fn open(db: &Database) -> std::io::Result<Self> {
        // Provision the immutable collection before any transaction uses it.
        // Its built-in unique _id enforces one batch per tenant/org/plan.
        terms_store::ensure_collection(db, BATCHES)
            .await
            .map_err(|_| std::io::Error::other("Cannot initialize Procurement purchase batches"))?;
        Ok(Self(
            db.collection_with_options(BATCHES, majority_collection()),
        ))
    }

    pub(super) async fn get(&self, scope: Scope, plan_id: Uuid) -> Result<Batch, IssueFailure> {
        self.0
            .find_one(key(scope, plan_id))
            .max_time(DB_TIMEOUT)
            .await
            .map_err(|_| IssueFailure::Unavailable)?
            .ok_or(IssueFailure::NotFound)?
            .checked_batch(plan_id)
    }

    pub(super) async fn exists(
        &self,
        scope: Scope,
        plan_id: Uuid,
        session: Option<&mut ClientSession>,
    ) -> Result<bool, MongoError> {
        let collection = self.0.clone_with_type::<Document>();
        let find = collection
            .find_one(key(scope, plan_id))
            .projection(doc! {"_id": 1})
            .max_time(DB_TIMEOUT);
        let batch = match session {
            Some(session) => find.session(session).await?,
            None => find.await?,
        };
        Ok(batch.is_some())
    }

    pub(super) async fn issue(
        &self,
        issue: Issue<'_>,
        deadline: Instant,
    ) -> Result<Saved, IssueFailure> {
        let scope = issue.identity.into();
        issue
            .terms
            .guard()
            .write(
                (scope, issue.plan_id),
                deadline,
                IssueFailure::Unavailable,
                &(self, issue),
                |(store, issue), session| Box::pin(store.apply(issue, session)),
            )
            .await
    }

    async fn apply(
        &self,
        issue: &Issue<'_>,
        session: &mut ClientSession,
    ) -> Result<Saved, TransactionError<IssueFailure>> {
        let scope = issue.identity.into();
        let plan_id = issue.plan_id;
        // Only the authorized original actor/command/intent can replay. Do not
        // re-form the batch or require the old policy to remain configured.
        if let Some(record) = self
            .0
            .find_one(key(scope, plan_id))
            .max_time(DB_TIMEOUT)
            .session(&mut *session)
            .await?
        {
            return record
                .checked_batch(plan_id)
                .and_then(|batch| batch.replay(issue.intent, issue.identity.actor))
                .map(Saved::Replay)
                .map_err(TransactionError::Rule);
        }
        let approved_terms = issue
            .terms
            .latest_in(scope, plan_id, session)
            .await?
            .ok_or(TransactionError::Rule(IssueFailure::NotFound))?;
        let attempt = Issuance::new(
            scope,
            &approved_terms,
            issue.policy,
            issue.intent,
            issue.identity.actor,
        )
        .map_err(TransactionError::Rule)?;
        let approval_decisions = issue
            .approvals
            .decisions(scope, plan_id, attempt.version(), session)
            .await?;
        let draft = issue
            .drafts
            .get_in(scope, plan_id, session)
            .await?
            .ok_or(TransactionError::Rule(IssueFailure::Unavailable))?;
        let batch = attempt
            .form(&draft, &approval_decisions)
            .map_err(TransactionError::Rule)?;
        let record = Record {
            schema_version: 1,
            batch,
            approved_terms,
            approval_decisions,
        };
        // The batch is also the command receipt. All assigned IDs, historical
        // values and the exact authorization/approval provenance commit together.
        let document = record.to_document(scope).map_err(TransactionError::Rule)?;
        self.0
            .clone_with_type::<Document>()
            .insert_one(document)
            .session(session)
            .await?;
        Ok(Saved::Created(record.batch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::purchase_batch::{
        IssuedBatch,
        tests::{ISSUER, approval_fixture},
    };

    #[test]
    fn immutable_bson_batch_is_scoped_and_carries_exact_private_audit_and_replay() {
        let (scope, draft, terms, policy, intent, approvals) = approval_fixture();
        let plan_id = draft.plan.plan_id;
        let batch = Issuance::new(scope, &terms, Some(&policy), intent, ISSUER)
            .unwrap()
            .form(&draft, &approvals)
            .unwrap();
        let record = Record {
            schema_version: 1,
            batch,
            approved_terms: terms,
            approval_decisions: approvals.into(),
        };
        let doc = record.to_document(scope).unwrap();
        assert_eq!(
            doc.get_document("_id"),
            Ok(&doc! {
                "tenantId": scope.tenant_id.to_string(),
                "organizationId": scope.organization_id.to_string(),
                "planId": plan_id.to_string(),
            })
        );
        assert_ne!(key(scope, plan_id), key(scope, Uuid::new_v4()));
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
            assert_ne!(key(scope, plan_id), key(foreign, plan_id));
        }
        assert_eq!(doc.get_i32("schemaVersion"), Ok(1));
        let saved_batch = doc.get_document("batch").unwrap();
        let origin = saved_batch.get_document("origin").unwrap();
        assert_eq!(origin.get_str("actor_id").unwrap(), ISSUER.to_string());
        assert!(origin.get_datetime("issued_at").is_ok());
        assert_eq!(
            origin.get_document("intent").unwrap(),
            &bson::to_document(&intent).unwrap()
        );
        assert_eq!(saved_batch.get_str("total"), Ok("1882.40"));
        assert_eq!(saved_batch.get_array("orders").unwrap().len(), 2);
        let saved_terms = doc.get_document("approvedTerms").unwrap();
        assert_eq!(
            saved_terms.get_str("actorId").unwrap(),
            record.approved_terms.actor_id.to_string()
        );
        assert!(saved_terms.get_datetime("savedAt").is_ok());
        let approvals = doc.get_array("approvalDecisions").unwrap();
        assert_eq!(approvals.len(), 2);
        assert!(
            approvals[1]
                .as_document()
                .unwrap()
                .get_document("policySnapshot")
                .is_ok()
        );

        let encoded = bson::to_vec(&doc).unwrap();
        let restore = || bson::from_slice::<Record>(&encoded).unwrap();
        assert_eq!(restore(), record);
        let original = serde_json::to_value(IssuedBatch::try_from(record.batch).unwrap()).unwrap();
        // This is retrieved only from the stored receipt, even if policy is
        // removed or changed after issuance. No IDs/values are reassigned.
        let replay = restore()
            .checked_batch(plan_id)
            .unwrap()
            .replay(intent, ISSUER)
            .unwrap();
        assert_eq!(
            serde_json::to_value(IssuedBatch::try_from(replay).unwrap()).unwrap(),
            original
        );

        for (intent, actor) in [
            (intent, Uuid::from_u128(11)),
            (
                Intent {
                    command_id: Uuid::new_v4(),
                    ..intent
                },
                ISSUER,
            ),
            (
                Intent {
                    terms_revision: 2,
                    ..intent
                },
                ISSUER,
            ),
            (
                Intent {
                    policy_id: Uuid::new_v4(),
                    ..intent
                },
                ISSUER,
            ),
            (
                Intent {
                    policy_revision: 2,
                    ..intent
                },
                ISSUER,
            ),
        ] {
            assert_eq!(
                restore()
                    .checked_batch(plan_id)
                    .unwrap()
                    .replay(intent, actor),
                Err(IssueFailure::Conflict)
            );
        }
        assert_eq!(
            restore().checked_batch(Uuid::new_v4()),
            Err(IssueFailure::Unavailable)
        );
        let mut incompatible = restore();
        incompatible.schema_version = 2;
        assert_eq!(
            incompatible.checked_batch(plan_id),
            Err(IssueFailure::Unavailable)
        );
    }
}
