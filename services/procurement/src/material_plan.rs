use crate::Scope;
use erp4us_messaging::{
    Contract, Delivery, Envelope, Error, HeaderMap, Kind, Layer, Route, Scope as SubjectScope,
};
use serde::{Deserialize, Deserializer, Serialize, de};
use std::collections::HashSet;
use uuid::Uuid;

pub(super) const STREAM: &str = "ERP4US_BUSINESS_EVENTS_V1";
const SOURCE: &str = "erp4us.inventory";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PlanNetted {
    #[serde(with = "canonical_id")]
    pub plan_id: Uuid,
    #[serde(with = "canonical_id")]
    pub order_id: Uuid,
    #[serde(with = "canonical_id")]
    pub line_id: Uuid,
    pub bom: Bom,
    pub buy_rows: Vec<BuyRow>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Bom {
    sku: String,
    revision: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct BuyRow {
    sku: String,
    #[serde(deserialize_with = "required_revision")]
    revision: Option<String>,
    unit: Unit,
    pieces: String,
}

impl BuyRow {
    pub(super) fn key(&self) -> (&str, Option<&str>) {
        (&self.sku, self.revision.as_deref())
    }

    pub(super) fn whole_pieces(&self) -> Option<u64> {
        pieces(&self.pieces)
            .then(|| self.pieces.parse().ok())
            .flatten()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum Unit {
    #[serde(rename = "EA")]
    Each,
}

// Missing and explicitly null are different on the wire.
pub(super) fn required_revision<'de, D: Deserializer<'de>>(
    input: D,
) -> Result<Option<String>, D::Error> {
    Option::deserialize(input)
}

pub(super) fn sku(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
}

pub(super) fn revision(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub(super) fn pieces(value: &str) -> bool {
    (1..=7).contains(&value.len())
        && !value.starts_with('0')
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u32>().is_ok_and(|n| n <= 1_000_000)
}

pub(super) fn canonical_uuid(value: &str) -> Option<Uuid> {
    Uuid::try_parse(value)
        .ok()
        .filter(|id| !id.is_nil() && id.hyphenated().to_string() == value)
}

pub(super) mod canonical_id {
    use super::*;

    pub fn deserialize<'de, D: Deserializer<'de>>(input: D) -> Result<Uuid, D::Error> {
        canonical_uuid(&String::deserialize(input)?)
            .ok_or_else(|| de::Error::custom("expected a nonzero canonical UUID"))
    }

    pub use uuid::serde::hyphenated::serialize;
}

impl Contract for PlanNetted {
    const ROUTE: Route<'static> = Route {
        layer: Layer::Business,
        domain: "inventory",
        kind: Kind::Event,
        resource: "material-plan",
        action: "netted",
    };
    const SCHEMA_VERSION: &'static str = "1.0.0";
    const PROTOCOL_SUPPORT: &'static str = "=1.0.0";
    const SCHEMA_SUPPORT: &'static str = "=1.0.0";
    const DELIVERY: Delivery = Delivery::JetStream(STREAM);

    fn validate(&self) -> Result<(), Error> {
        if self.plan_id.is_nil()
            || self.order_id.is_nil()
            || self.line_id.is_nil()
            || !sku(&self.bom.sku)
            || !revision(&self.bom.revision)
            || !(1..=32).contains(&self.buy_rows.len())
        {
            return Err(Error::InvalidEnvelope("material plan payload"));
        }
        let mut seen = HashSet::with_capacity(self.buy_rows.len());
        if !self.buy_rows.iter().all(|row| {
            sku(&row.sku)
                && row.revision.as_deref().is_none_or(revision)
                && pieces(&row.pieces)
                && seen.insert((row.sku.as_str(), row.revision.as_deref()))
        }) {
            return Err(Error::InvalidEnvelope("material plan buy rows"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Origin {
    #[serde(with = "canonical_id")]
    actor_id: Uuid,
    actor_kind: String,
    #[serde(with = "canonical_id")]
    correlation_id: Uuid,
    #[serde(with = "canonical_id")]
    causation_id: Uuid,
    timestamp: String,
    service_version: String,
}

pub(super) fn subject(scope: Scope) -> Result<String, Error> {
    PlanNetted::ROUTE.subject(1, SubjectScope::Tenant(scope.tenant_id), 1)
}

pub(super) struct AuthorizedPlan {
    pub scope: Scope,
    pub message_id: Uuid,
    pub plan: PlanNetted,
    pub origin: Origin,
}

pub(super) fn decode(
    scope: Scope,
    expected_subject: &str,
    subject: &str,
    headers: Option<&HeaderMap>,
    payload: &[u8],
) -> Option<AuthorizedPlan> {
    if subject != expected_subject {
        return None;
    }
    let mut event = Envelope::<PlanNetted>::decode(subject, headers?, payload).ok()?;
    if event.source_service != SOURCE
        || event.tenant_id != Some(scope.tenant_id)
        || event.organization_id != Some(scope.organization_id)
    {
        return None;
    }
    let actor = event.actor?;
    let origin = Origin {
        actor_id: canonical_uuid(&actor.id)?,
        actor_kind: actor.kind,
        correlation_id: event.correlation_id,
        causation_id: event.causation_id?,
        timestamp: event.timestamp,
        service_version: event.service_version.to_string(),
    };
    event
        .payload
        .buy_rows
        .sort_unstable_by(|a, b| (&a.sku, &a.revision).cmp(&(&b.sku, &b.revision)));
    Some(AuthorizedPlan {
        scope,
        message_id: event.message_id,
        plan: event.payload,
        origin,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use serde_json::{Value, json};

    // Independent v1 wire sample: no runtime producer or shared Rust payload type.
    const SUBJECT: &str =
        "v1.business.ddcae139-c1e8-47d2-8b48-349cb6f30ce5.inventory.event.material-plan.netted.v1";

    pub fn sample() -> Value {
        json!({
            "messageType": SUBJECT,
            "messageId": "e44a77a1-9a2c-4c09-97a1-d509ad7b602c",
            "correlationId": "ae27188c-c804-4b3c-a85a-678d1f570dba",
            "causationId": "ae27188c-c804-4b3c-a85a-678d1f570dba",
            "protocolVersion": "1.0.0",
            "schemaVersion": "1.0.0",
            "serviceVersion": "0.1.0",
            "timestamp": "2026-11-28T08:00:00Z",
            "sourceService": "erp4us.inventory",
            "tenantId": "ddcae139-c1e8-47d2-8b48-349cb6f30ce5",
            "organizationId": "3e793fb5-75a7-45f6-8f66-45a58fcec674",
            "actor": {"id": "142a290f-4bbc-438e-aefe-e3697a8bcd16", "kind": "user"},
            "payload": {
                "planId": "bcccc5c5-f640-4142-a722-f730f1fdc9b0",
                "orderId": "acf27f04-dfdf-445b-b38d-55514cc9c433",
                "lineId": "5b83b773-f8b2-42e0-b995-64d565c0ae2d",
                "bom": {"sku": "NM-1000", "revision": "A"},
                "buyRows": [
                    {"sku": "NM-SPINDLE", "revision": "B", "unit": "EA", "pieces": "2"},
                    {"sku": "NM-SPINDLE", "revision": null, "unit": "EA", "pieces": "10"},
                    {"sku": "NM-MOTOR", "revision": null, "unit": "EA", "pieces": "6"}
                ]
            }
        })
    }

    fn scope() -> Scope {
        Scope {
            tenant_id: Uuid::parse_str("ddcae139-c1e8-47d2-8b48-349cb6f30ce5").unwrap(),
            organization_id: Uuid::parse_str("3e793fb5-75a7-45f6-8f66-45a58fcec674").unwrap(),
        }
    }

    fn headers(sample: &Value) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", "application/json");
        for (header, key) in [
            ("Erp4us-Protocol-Version", "protocolVersion"),
            ("Erp4us-Schema-Version", "schemaVersion"),
            ("Erp4us-Service-Version", "serviceVersion"),
            ("Erp4us-Message-Id", "messageId"),
            ("Erp4us-Correlation-Id", "correlationId"),
            ("Erp4us-Causation-Id", "causationId"),
            ("Erp4us-Source", "sourceService"),
            ("Erp4us-Tenant-Id", "tenantId"),
            ("Erp4us-Organization-Id", "organizationId"),
            ("Nats-Msg-Id", "messageId"),
        ] {
            if let Some(value) = sample[key].as_str() {
                headers.insert(header, value);
            }
        }
        headers
    }

    pub fn read(sample: &Value) -> Option<AuthorizedPlan> {
        decode(
            scope(),
            SUBJECT,
            SUBJECT,
            Some(&headers(sample)),
            &serde_json::to_vec(sample).unwrap(),
        )
    }

    #[test]
    fn authorizes_exact_route_origin_scope_and_versions() {
        assert_eq!(subject(scope()).unwrap(), SUBJECT);
        assert!(read(&sample()).is_some());
        for (key, value) in [
            ("tenantId", json!(Uuid::new_v4())),
            ("organizationId", json!(Uuid::new_v4())),
            ("sourceService", json!("erp4us.sales")),
            ("actor", json!({"id": Uuid::nil(), "kind": "user"})),
            ("actor", json!({"id": Uuid::new_v4(), "kind": "unknown"})),
            ("actor", Value::Null),
            ("causationId", Value::Null),
            ("schemaVersion", json!("1.0.1")),
            ("protocolVersion", json!("1.1.0")),
        ] {
            let mut invalid = sample();
            invalid[key] = value;
            assert!(read(&invalid).is_none(), "{key}");
        }
        let sample = sample();
        let bytes = serde_json::to_vec(&sample).unwrap();
        let mut bad_headers = headers(&sample);
        bad_headers.insert("Erp4us-Source", "erp4us.sales");
        assert!(decode(scope(), SUBJECT, SUBJECT, Some(&bad_headers), &bytes).is_none());
        let scope_token = scope().tenant_id.to_string();
        let foreign_subject = SUBJECT.replace(&scope_token, &Uuid::new_v4().to_string());
        assert!(
            decode(
                scope(),
                SUBJECT,
                &foreign_subject,
                Some(&headers(&sample)),
                &bytes
            )
            .is_none()
        );
    }

    #[test]
    fn rejects_invalid_positive_buy_rows_and_duplicates() {
        for field in [
            json!("0"),
            json!("01"),
            json!("1.0"),
            json!("+1"),
            json!("1\n"),
            json!(1),
            json!("1000001"),
        ] {
            let mut invalid = sample();
            invalid["payload"]["buyRows"][0]["pieces"] = field;
            assert!(read(&invalid).is_none());
        }
        for (key, value) in [
            ("unit", json!("KG")),
            ("sku", json!("nm-motor")),
            ("revision", json!("A/2")),
            ("revision", json!("A".repeat(33))),
        ] {
            let mut invalid = sample();
            invalid["payload"]["buyRows"][0][key] = value;
            assert!(read(&invalid).is_none(), "{key}");
        }
        let mut event = sample();
        event["payload"]["buyRows"][0]["pieces"] = json!("1000000");
        assert!(read(&event).is_some());
        event["payload"]["buyRows"][0]["revision"] = Value::Null;
        assert!(read(&event).is_none(), "duplicate SKU and revision");
        event["payload"]["buyRows"][0]
            .as_object_mut()
            .unwrap()
            .remove("revision");
        assert!(read(&event).is_none(), "missing nullable revision");
        event["payload"]["buyRows"] = json!([]);
        assert!(read(&event).is_none());
        event["payload"]["buyRows"] = json!((0..33)
            .map(|n| json!({"sku": format!("PART-{n}"), "revision": null, "unit": "EA", "pieces": "1"}))
            .collect::<Vec<_>>());
        assert!(read(&event).is_none());
        event["payload"]["buyRows"].as_array_mut().unwrap().pop();
        assert!(read(&event).is_some());
    }
}
