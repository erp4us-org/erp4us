//! Verify the proposed receipt wire fixture independently of Inventory types.
//! This does not configure a consumer or authorize publication.

use crate::{
    Scope,
    material_plan::{STREAM, canonical_id, pieces, required_revision, revision, sku},
};
use erp4us_messaging::{
    Contract, Delivery, Envelope, Error, HeaderMap, Kind, Layer, Route, Scope as SubjectScope,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub(super) const MAX_BODY_BYTES: usize = 7 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct ReceiptPosted {
    #[serde(with = "canonical_id")]
    pub receipt_id: Uuid,
    #[serde(with = "canonical_id")]
    pub plan_id: Uuid,
    #[serde(with = "canonical_id")]
    pub purchase_order_id: Uuid,
    #[serde(with = "canonical_id")]
    pub purchase_line_id: Uuid,
    pub sku: String,
    #[serde(deserialize_with = "required_revision")]
    pub revision: Option<String>,
    pub unit: String,
    pub pieces: String,
    pub receipt_sequence: u32,
}

impl Contract for ReceiptPosted {
    const ROUTE: Route<'static> = Route {
        layer: Layer::Business,
        domain: "inventory",
        kind: Kind::Event,
        resource: "purchase-receipt",
        action: "posted",
    };
    const SCHEMA_VERSION: &'static str = "1.0.0";
    const PROTOCOL_SUPPORT: &'static str = "=1.0.0";
    const SCHEMA_SUPPORT: &'static str = "=1.0.0";
    const DELIVERY: Delivery = Delivery::JetStream(STREAM);

    fn validate(&self) -> Result<(), Error> {
        if self.unit != "EA"
            || !sku(&self.sku)
            || self
                .revision
                .as_deref()
                .is_some_and(|value| !revision(value))
            || !pieces(&self.pieces)
            || !(1..=1_000_000).contains(&self.receipt_sequence)
        {
            return Err(Error::InvalidEnvelope("purchase receipt payload"));
        }
        Ok(())
    }
}

/// `scope` must come from the consumer's trusted tenant + organization policy.
/// This is message validation, not establishment of an Inventory broker identity.
pub(super) fn decode(
    scope: Scope,
    subject: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Option<Envelope<ReceiptPosted>> {
    if body.len() > MAX_BODY_BYTES
        || ReceiptPosted::ROUTE
            .subject(1, SubjectScope::Tenant(scope.tenant_id), 1)
            .ok()?
            != subject
    {
        return None;
    }
    let event = Envelope::<ReceiptPosted>::decode(subject, headers, body).ok()?;
    if event.source_service != "erp4us.inventory"
        || event.tenant_id != Some(scope.tenant_id)
        || event.organization_id != Some(scope.organization_id)
        || event.message_id != event.correlation_id
        || event.message_id == event.payload.receipt_id
        || event.causation_id.is_some()
        || event.actor.is_none()
    {
        return None;
    }
    // The generic envelope treats a null optional value like omission. This
    // root event requires omission and an allowlisted actor shape on the wire.
    let raw: Value = serde_json::from_slice(body).ok()?;
    let fields = raw.as_object()?;
    let actor = fields.get("actor")?.as_object()?;
    if fields.contains_key("causationId") || actor.len() != 2 {
        return None;
    }
    Some(event)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use serde_json::json;

    const FIXTURE: &str =
        include_str!("../../../contracts/examples/nats/inventory-purchase-receipt-posted-v1.json");

    pub(crate) fn fixture() -> (Scope, String, HeaderMap, Value) {
        let mut sample: Value = serde_json::from_str(FIXTURE).unwrap();
        let event = sample["envelope"].take();
        let scope = Scope {
            tenant_id: serde_json::from_value(event["tenantId"].clone()).unwrap(),
            organization_id: serde_json::from_value(event["organizationId"].clone()).unwrap(),
        };
        let mut headers = HeaderMap::new();
        for (key, value) in sample["headers"].as_object().unwrap() {
            headers.insert(key.as_str(), value.as_str().unwrap());
        }
        (
            scope,
            sample["subject"].as_str().unwrap().into(),
            headers,
            event,
        )
    }

    fn materialize(event: &Value) -> Vec<u8> {
        serde_json::to_vec(event).unwrap()
    }

    #[test]
    fn owner_sample_and_operator_replay_keep_the_same_business_fact() {
        let (scope, subject, mut headers, fixture) = fixture();
        let bytes = materialize(&fixture);
        let first = decode(scope, &subject, &headers, &bytes).unwrap();
        assert_eq!(first.payload.sku, "NM-ENDPLATE");
        assert_eq!(first.payload.pieces, "6");
        assert_eq!(first.payload.receipt_sequence, 1);
        assert_eq!(first.payload.revision, None);
        assert_eq!(first.correlation_id, first.message_id);

        headers.insert(
            "Nats-Msg-Id",
            format!("replay-38-{}", Uuid::new_v4()).as_str(),
        );
        assert!(decode(scope, &subject, &headers, &bytes).is_none());
        headers.insert(
            "Nats-Msg-Id",
            format!("replay-38-{}", first.message_id).as_str(),
        );
        let replay = decode(scope, &subject, &headers, &bytes).unwrap();
        assert_eq!(
            serde_json::to_value(replay).unwrap(),
            serde_json::to_value(first).unwrap()
        );
    }

    #[test]
    fn denies_mismatched_scope_or_route_even_for_a_well_formed_event() {
        let (scope, subject, headers, fixture) = fixture();
        let bytes = materialize(&fixture);
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
            assert!(decode(foreign, &subject, &headers, &bytes).is_none());
        }
        assert!(decode(scope, &subject.replace(".v1", ".v2"), &headers, &bytes).is_none());
        let mut conflicting = headers;
        conflicting.insert("Erp4us-Source", "erp4us.sales");
        assert!(decode(scope, &subject, &conflicting, &bytes).is_none());
    }

    #[test]
    fn denies_undeclared_fields_noncanonical_values_and_nonroot_identity() {
        let (scope, subject, mut headers, fixture) = fixture();
        for (key, value) in [
            ("pieces", json!("06")),
            ("pieces", json!(6)),
            ("receiptSequence", json!(0)),
            ("revision", json!("a/b")),
            ("unit", json!("KG")),
            ("price", json!("30.10")),
            ("receiptId", fixture["messageId"].clone()),
        ] {
            let mut invalid = fixture.clone();
            invalid["payload"][key] = value;
            assert!(
                decode(scope, &subject, &headers, &materialize(&invalid)).is_none(),
                "{key}"
            );
        }
        let mut invalid = fixture.clone();
        invalid["payload"]
            .as_object_mut()
            .unwrap()
            .remove("revision");
        assert!(decode(scope, &subject, &headers, &materialize(&invalid)).is_none());
        let mut invalid = fixture.clone();
        invalid["causationId"] = Value::Null;
        assert!(decode(scope, &subject, &headers, &materialize(&invalid)).is_none());
        let mut invalid = fixture.clone();
        invalid["actor"]["displayName"] = json!("not part of the fact");
        assert!(decode(scope, &subject, &headers, &materialize(&invalid)).is_none());
        let other = Uuid::new_v4().to_string();
        headers.insert("Erp4us-Correlation-Id", other.as_str());
        invalid = fixture;
        invalid["correlationId"] = json!(other);
        assert!(decode(scope, &subject, &headers, &materialize(&invalid)).is_none());
    }
}
