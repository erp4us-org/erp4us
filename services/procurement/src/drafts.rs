use crate::{
    Scope,
    material_plan::{AuthorizedPlan, Origin, PlanNetted, STREAM, canonical_id},
    required,
};
use mongodb::{
    Client, ClientSession, Database,
    bson::{DateTime, Document, doc},
    error::UNKNOWN_TRANSACTION_COMMIT_RESULT,
    options::{
        Acknowledgment, CollectionOptions, ReadConcern, ReadPreference, SelectionCriteria,
        WriteConcern,
    },
};
use serde::{Deserialize, Serialize};
use std::{io, time::Duration};
use uuid::Uuid;

const DRAFTS: &str = "material_plan_drafts";
const INBOX: &str = "material_plan_inbox";
const QUARANTINE: &str = "material_plan_quarantine";
const DB_TIME: Duration = Duration::from_secs(3);

fn primary_majority() -> CollectionOptions {
    CollectionOptions::builder()
        .read_concern(ReadConcern::majority())
        .selection_criteria(SelectionCriteria::ReadPreference(ReadPreference::Primary))
        .build()
}

fn majority_write() -> WriteConcern {
    WriteConcern::builder()
        .w(Acknowledgment::Majority)
        .w_timeout(DB_TIME)
        .build()
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Failure {
    ConflictingMessage,
    ConflictingPlan,
    Unavailable,
}

impl From<mongodb::error::Error> for Failure {
    fn from(_: mongodb::error::Error) -> Self {
        Self::Unavailable
    }
}

#[derive(Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    #[serde(with = "canonical_id")]
    plan_id: Uuid,
    origin: Origin,
    schema_version: i32,
}

impl Receipt {
    fn of(event: &AuthorizedPlan) -> Self {
        Self {
            plan_id: event.plan.plan_id,
            origin: event.origin.clone(),
            schema_version: 1,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Status {
    Draft,
}

#[derive(Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Draft {
    #[serde(with = "canonical_id")]
    pub proposal_id: Uuid,
    pub status: Status,
    pub plan: PlanNetted,
    #[serde(with = "canonical_id")]
    source_event_id: Uuid,
    origin: Origin,
    created_at: DateTime,
    schema_version: i32,
}

impl Draft {
    pub(super) fn new(event: &AuthorizedPlan) -> Self {
        Self {
            proposal_id: Uuid::new_v4(),
            status: Status::Draft,
            plan: event.plan.clone(),
            source_event_id: event.message_id,
            origin: event.origin.clone(),
            created_at: DateTime::now(),
            schema_version: 1,
        }
    }
}

fn scoped_key(scope: Scope, field: &str, id: Uuid) -> Document {
    doc! {
        "tenantId": scope.tenant_id.to_string(),
        "organizationId": scope.organization_id.to_string(),
        field: id.to_string(),
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Apply {
    None,
    ReceiptOnly,
    DraftAndReceipt,
}

fn decide(
    plan: &PlanNetted,
    receipt: &Receipt,
    stored_draft: Option<&Draft>,
    stored_receipt: Option<&Receipt>,
) -> Result<Apply, Failure> {
    if let Some(old) = stored_receipt
        && old != receipt
    {
        return Err(Failure::ConflictingMessage);
    }

    match (stored_draft, stored_receipt) {
        (Some(draft), _) if draft.plan != *plan => Err(Failure::ConflictingPlan),
        (Some(_), Some(_)) => Ok(Apply::None),
        (Some(_), None) => Ok(Apply::ReceiptOnly),
        (None, None) => Ok(Apply::DraftAndReceipt),
        // A receipt cannot exist without its draft after an atomic commit. Don't replace it.
        (None, Some(_)) => Err(Failure::Unavailable),
    }
}

#[derive(Clone)]
pub(super) struct Store(Database);

impl Store {
    pub async fn open() -> io::Result<Self> {
        let client = Client::with_uri_str(required("PROCUREMENT_MONGO_URI")?)
            .await
            .map_err(|_| io::Error::other("Invalid Procurement database configuration"))?;
        Ok(Self(client.database("procurement")))
    }

    pub async fn ready(&self) -> bool {
        self.0.run_command(doc! {"ping": 1}).await.is_ok()
    }

    pub(super) async fn open_terms(&self) -> io::Result<crate::terms_store::Store> {
        crate::terms_store::Store::open(&self.0).await
    }

    pub(super) async fn open_approvals(&self) -> io::Result<crate::approval_store::Store> {
        crate::approval_store::Store::open(&self.0).await
    }

    pub async fn get(&self, scope: Scope, plan_id: Uuid) -> Result<Option<Draft>, Failure> {
        Ok(self
            .0
            .collection_with_options::<Draft>(DRAFTS, primary_majority())
            .find_one(doc! {"_id": scoped_key(scope, "planId", plan_id)})
            .max_time(DB_TIME)
            .await?)
    }

    pub(super) async fn get_in(
        &self,
        scope: Scope,
        plan_id: Uuid,
        session: &mut ClientSession,
    ) -> Result<Option<Draft>, mongodb::error::Error> {
        self.0
            .collection::<Draft>(DRAFTS)
            .find_one(doc! {"_id": scoped_key(scope, "planId", plan_id)})
            .max_time(DB_TIME)
            .session(session)
            .await
    }

    pub async fn record(&self, event: &AuthorizedPlan) -> Result<(), Failure> {
        let mut session = self.0.client().start_session().await?;
        session
            .start_transaction()
            .read_concern(ReadConcern::snapshot())
            .write_concern(majority_write())
            .max_commit_time(DB_TIME)
            .await?;

        if let Err(failure) = self.apply(event, &mut session).await {
            // No commit was sent. Await abort rather than starting cleanup on session drop.
            session.abort_transaction().await?;
            return Err(failure);
        }
        // This also commits the read-only replay path: a snapshot read is majority
        // committed when its transaction commits with majority write concern.
        confirm_commit(&mut session).await
    }

    async fn apply(
        &self,
        event: &AuthorizedPlan,
        session: &mut ClientSession,
    ) -> Result<(), Failure> {
        let drafts = self.0.collection::<Draft>(DRAFTS);
        let inbox = self.0.collection::<Receipt>(INBOX);
        let draft_key = scoped_key(event.scope, "planId", event.plan.plan_id);
        let receipt_key = scoped_key(event.scope, "messageId", event.message_id);
        let receipt = Receipt::of(event);

        let stored_receipt = inbox
            .find_one(doc! {"_id": &receipt_key})
            .max_time(DB_TIME)
            .session(&mut *session)
            .await?;
        let stored_draft = drafts
            .find_one(doc! {"_id": &draft_key})
            .max_time(DB_TIME)
            .session(&mut *session)
            .await?;
        let apply = decide(
            &event.plan,
            &receipt,
            stored_draft.as_ref(),
            stored_receipt.as_ref(),
        )?;
        if apply == Apply::DraftAndReceipt {
            self.0
                .collection::<Document>(DRAFTS)
                .insert_one(record(draft_key, &Draft::new(event))?)
                .session(&mut *session)
                .await?;
        }
        if apply != Apply::None {
            self.0
                .collection::<Document>(INBOX)
                .insert_one(record(receipt_key, &receipt)?)
                .session(session)
                .await?;
        }
        Ok(())
    }

    pub async fn quarantine(
        &self,
        stream_sequence: u64,
        delivery_count: i64,
        reason: &'static str,
    ) -> Result<(), Failure> {
        let key = doc! {"_id": format!("{STREAM}:{stream_sequence}")};
        let quarantine = self
            .0
            .collection_with_options::<Document>(QUARANTINE, primary_majority());
        quarantine
            .find_one_and_update(
                key.clone(),
                doc! {"$setOnInsert": quarantine_record(delivery_count, reason)},
            )
            .upsert(true)
            .max_time(DB_TIME)
            .write_concern(majority_write())
            .await?;
        // On replay $setOnInsert can be a no-op. Prove the reference is visible
        // to a majority read, not just matched by the upsert on this primary.
        quarantine
            .find_one(key)
            .projection(doc! {"_id": 1})
            .max_time(DB_TIME)
            .await?
            .map(|_| ())
            .ok_or(Failure::Unavailable)
    }
}

async fn confirm_commit(session: &mut ClientSession) -> Result<(), Failure> {
    let mut delay = Duration::from_secs(1);
    loop {
        match session.commit_transaction().await {
            // A write concern timeout or lost reply may have left the draft committed.
            // Retry only this commit, on the same session, not the business operation.
            Err(error) if error.contains_label(UNKNOWN_TRANSACTION_COMMIT_RESULT) => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(8));
            }
            result => return result.map_err(Failure::from),
        }
    }
}

fn record<T: Serialize>(key: Document, model: &T) -> Result<Document, Failure> {
    let mut doc = mongodb::bson::to_document(model).map_err(|_| Failure::Unavailable)?;
    doc.insert("_id", key);
    Ok(doc)
}

fn quarantine_record(delivery_count: i64, reason: &'static str) -> Document {
    doc! {
        "reason": reason,
        "deliveryCount": delivery_count,
        "recordedAt": DateTime::now(),
        "schemaVersion": 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::material_plan::tests::{read, sample};
    use mongodb::bson::{self, Bson};
    use serde_json::json;

    #[test]
    fn normalized_replays_keep_the_draft_and_detect_conflicts() {
        let mut event = sample();
        let original = read(&event).unwrap();
        let draft = Draft::new(&original);
        let receipt = Receipt::of(&original);
        assert_eq!(
            decide(&original.plan, &receipt, None, None),
            Ok(Apply::DraftAndReceipt)
        );
        assert_eq!(
            decide(&original.plan, &receipt, Some(&draft), Some(&receipt)),
            Ok(Apply::None)
        );

        // A fresh fact may have different event metadata but may not replace its plan.
        event["messageId"] = json!(Uuid::new_v4());
        event["actor"]["id"] = json!(Uuid::new_v4());
        event["payload"]["buyRows"]
            .as_array_mut()
            .unwrap()
            .reverse();
        let fresh = read(&event).unwrap();
        let fresh_receipt = Receipt::of(&fresh);
        assert_eq!(original.plan, fresh.plan);
        assert_eq!(
            decide(&fresh.plan, &fresh_receipt, Some(&draft), None),
            Ok(Apply::ReceiptOnly)
        );
        assert_eq!(
            decide(&fresh.plan, &fresh_receipt, Some(&draft), Some(&receipt)),
            Err(Failure::ConflictingMessage)
        );

        event["payload"]["buyRows"][0]["pieces"] = json!("9");
        let changed = read(&event).unwrap();
        assert_eq!(
            decide(&changed.plan, &Receipt::of(&changed), Some(&draft), None),
            Err(Failure::ConflictingPlan)
        );
        assert_eq!(
            decide(&fresh.plan, &fresh_receipt, None, Some(&fresh_receipt)),
            Err(Failure::Unavailable)
        );
    }

    #[test]
    fn bson_uses_scoped_string_uuid_keys_and_exact_sorted_snapshots() {
        let mut input = sample();
        input["payload"]["supplier"] = json!("not retained");
        input["payload"]["buyRows"][0]["price"] = json!("not retained");
        let event = read(&input).unwrap();
        let draft_key = scoped_key(event.scope, "planId", event.plan.plan_id);
        let draft = Draft::new(&event);
        let stored = record(draft_key.clone(), &draft).unwrap();
        assert_eq!(
            stored.get_document("_id").unwrap(),
            &doc! {
                "tenantId": input["tenantId"].as_str().unwrap(),
                "organizationId": input["organizationId"].as_str().unwrap(),
                "planId": input["payload"]["planId"].as_str().unwrap(),
            }
        );
        for foreign in [
            Scope {
                tenant_id: Uuid::new_v4(),
                ..event.scope
            },
            Scope {
                organization_id: Uuid::new_v4(),
                ..event.scope
            },
        ] {
            assert_ne!(draft_key, scoped_key(foreign, "planId", event.plan.plan_id));
        }
        assert_eq!(stored.get_str("status"), Ok("draft"));
        assert_eq!(stored.get_i32("schemaVersion"), Ok(1));
        assert!(stored.get_datetime("createdAt").is_ok());
        let plan = stored.get_document("plan").unwrap();
        assert_eq!(
            plan.get_str("planId"),
            Ok(input["payload"]["planId"].as_str().unwrap())
        );
        assert!(!plan.contains_key("supplier"));
        let rows = plan.get_array("buyRows").unwrap();
        assert_eq!(
            rows,
            &[
                Bson::Document(doc! {"sku":"NM-MOTOR","revision":null,"unit":"EA","pieces":"6"}),
                Bson::Document(doc! {"sku":"NM-SPINDLE","revision":null,"unit":"EA","pieces":"10"}),
                Bson::Document(doc! {"sku":"NM-SPINDLE","revision":"B","unit":"EA","pieces":"2"}),
            ]
        );
        assert_eq!(bson::from_document::<Draft>(stored).unwrap(), draft);

        let receipt = Receipt::of(&event);
        let inbox_id = scoped_key(event.scope, "messageId", event.message_id);
        let stored = record(inbox_id.clone(), &receipt).unwrap();
        assert_eq!(stored.get_document("_id"), Ok(&inbox_id));
        assert_eq!(
            inbox_id.get_str("messageId"),
            Ok(input["messageId"].as_str().unwrap())
        );
        let origin = stored.get_document("origin").unwrap();
        assert_eq!(
            origin.get_str("correlationId"),
            Ok(input["correlationId"].as_str().unwrap())
        );
        assert_eq!(
            origin.get_str("causationId"),
            Ok(input["causationId"].as_str().unwrap())
        );
        assert_eq!(
            origin.get_str("actorId"),
            Ok(input["actor"]["id"].as_str().unwrap())
        );
        assert_eq!(bson::from_document::<Receipt>(stored).unwrap(), receipt);
    }

    #[test]
    fn quarantine_details_are_payload_free() {
        let entry = quarantine_record(5, "retry-exhausted");
        assert_eq!(entry.len(), 4);
        assert_eq!(entry.get_str("reason"), Ok("retry-exhausted"));
        assert_eq!(entry.get_i64("deliveryCount"), Ok(5));
        assert_eq!(entry.get_i32("schemaVersion"), Ok(1));
        assert!(entry.get_datetime("recordedAt").is_ok());
    }
}
