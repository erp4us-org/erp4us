//! Validate the proposed non-commercial Procurement receipt source.
//! This test-gated boundary does not implement identity forwarding or an HTTP call.

use super::{IssuedLine, Organization, PurchaseLine, Reject};
use crate::netting::{Item, positive};
use erp4us_identity::Scope;
use serde::{Deserialize, Deserializer, de};
use uuid::Uuid;

const MAX_LINES: usize = 32;
const MAX_SOURCE_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    #[serde(deserialize_with = "canonical_id")]
    plan_id: Uuid,
    lines: Vec<Line>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    #[serde(deserialize_with = "canonical_id")]
    purchase_order_id: Uuid,
    #[serde(deserialize_with = "canonical_id")]
    purchase_line_id: Uuid,
    sku: String,
    #[serde(deserialize_with = "explicit_revision")]
    revision: Option<String>,
    unit: Each,
    ordered_pieces: String,
}

#[derive(Deserialize)]
enum Each {
    #[serde(rename = "EA")]
    Unit,
}

fn canonical_id<'de, D: Deserializer<'de>>(input: D) -> Result<Uuid, D::Error> {
    let text = String::deserialize(input)?;
    Uuid::try_parse(&text)
        .ok()
        .filter(|id| !id.is_nil() && id.hyphenated().to_string() == text)
        .ok_or_else(|| de::Error::custom("expected a nonzero canonical UUID"))
}

fn explicit_revision<'de, D: Deserializer<'de>>(input: D) -> Result<Option<String>, D::Error> {
    Option::deserialize(input)
}

#[derive(Debug)]
struct Selected {
    scope: Organization,
    actor: Uuid,
    plan: Uuid,
    purchase: PurchaseLine,
    item: Item,
    ordered: i32,
}

impl Selected {
    fn as_issued(&self) -> IssuedLine<'_> {
        IssuedLine {
            scope: self.scope,
            actor: self.actor,
            plan: self.plan,
            purchase: self.purchase,
            item: &self.item,
            unit: "EA",
            ordered: self.ordered,
        }
    }
}

/// Call only for an authenticated Procurement response obtained with the
/// same verified actor and organization. Shape is not proof of origin.
fn select(
    scope: Scope,
    plan: Uuid,
    requested: PurchaseLine,
    bytes: &[u8],
) -> Result<Selected, Reject> {
    if bytes.len() > MAX_SOURCE_BYTES
        || [scope.tenant, scope.organization, scope.actor, plan]
            .into_iter()
            .any(|id| id.is_nil())
    {
        return Err(Reject::InvalidSource);
    }
    let source: Source = serde_json::from_slice(bytes).map_err(|_| Reject::InvalidSource)?;
    if source.plan_id != plan || !(1..=MAX_LINES).contains(&source.lines.len()) {
        return Err(Reject::InvalidSource);
    }

    let mut previous = None;
    let mut selected = None;
    for line in source.lines {
        let pair = (line.purchase_order_id, line.purchase_line_id);
        if previous.is_some_and(|prior| prior >= pair) {
            return Err(Reject::InvalidSource);
        }
        previous = Some(pair);
        let item = Item {
            sku: line.sku,
            revision: line.revision,
        };
        let ordered = positive(&line.ordered_pieces).ok_or(Reject::InvalidSource)?;
        let Each::Unit = line.unit;
        if !item.valid() {
            return Err(Reject::InvalidSource);
        }
        if pair == (requested.order, requested.line) {
            selected = Some(Selected {
                scope: Organization {
                    tenant: scope.tenant,
                    organization: scope.organization,
                },
                actor: scope.actor,
                plan,
                purchase: requested,
                item,
                ordered,
            });
        }
    }
    selected.ok_or(Reject::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const SCOPE: Scope = Scope {
        tenant: Uuid::from_u128(1),
        organization: Uuid::from_u128(2),
        actor: Uuid::from_u128(3),
    };
    const PLAN: Uuid = Uuid::from_u128(4);
    const PURCHASE: PurchaseLine = PurchaseLine {
        order: Uuid::from_u128(5),
        line: Uuid::from_u128(6),
    };

    fn source() -> Value {
        json!({
            "plan_id": PLAN,
            "lines": [
                {"purchase_order_id": PURCHASE.order, "purchase_line_id": PURCHASE.line,
                    "sku":"NM-ENDPLATE", "revision":null, "unit":"EA", "ordered_pieces":"12"},
                {"purchase_order_id": Uuid::from_u128(7), "purchase_line_id": Uuid::from_u128(8),
                    "sku":"NM-CABLE", "revision":"A", "unit":"EA", "ordered_pieces":"10"},
            ]
        })
    }

    fn read(body: &Value, line: PurchaseLine) -> Result<Selected, Reject> {
        select(SCOPE, PLAN, line, &serde_json::to_vec(body).unwrap())
    }

    #[test]
    fn chooses_only_the_exact_tuple_and_carries_verified_scope_not_response_claims() {
        let source = source();
        let line = read(&source, PURCHASE).unwrap();
        let issued = line.as_issued();
        assert_eq!(issued.plan, PLAN);
        assert_eq!(issued.scope.tenant, SCOPE.tenant);
        assert_eq!(issued.scope.organization, SCOPE.organization);
        assert_eq!(issued.actor, SCOPE.actor);
        assert_eq!(issued.ordered, 12);
        assert_eq!(issued.item.sku, "NM-ENDPLATE");
        assert_eq!(issued.item.revision, None);
        assert_eq!(issued.unit, "EA");
        assert_eq!(
            read(
                &source,
                PurchaseLine {
                    order: Uuid::from_u128(7),
                    ..PURCHASE
                }
            )
            .unwrap_err(),
            Reject::NotFound
        );
    }

    #[test]
    fn rejects_invalid_pin_bounds_and_any_ambiguous_or_commercial_data() {
        for (key, value) in [
            ("unit", json!("KG")),
            ("ordered_pieces", json!("012")),
            ("sku", json!("nm-endplate")),
            ("revision", json!("A/B")),
            ("supplier", json!("not part of the source")),
            ("unit_price", json!("30.10")),
            ("purchase_line_id", json!(Uuid::nil())),
        ] {
            let mut invalid = source();
            invalid["lines"][0][key] = value;
            assert_eq!(read(&invalid, PURCHASE).unwrap_err(), Reject::InvalidSource);
        }
        let mut invalid = source();
        invalid["lines"][0]
            .as_object_mut()
            .unwrap()
            .remove("revision");
        assert_eq!(read(&invalid, PURCHASE).unwrap_err(), Reject::InvalidSource);
        let mut invalid = source();
        invalid["lines"][1] = invalid["lines"][0].clone();
        assert_eq!(read(&invalid, PURCHASE).unwrap_err(), Reject::InvalidSource);
        let mut invalid = source();
        invalid["lines"].as_array_mut().unwrap().reverse();
        assert_eq!(read(&invalid, PURCHASE).unwrap_err(), Reject::InvalidSource);
        let mut invalid = source();
        invalid["plan_id"] = json!(Uuid::from_u128(9));
        assert_eq!(read(&invalid, PURCHASE).unwrap_err(), Reject::InvalidSource);
        let mut invalid = serde_json::to_vec(&source()).unwrap();
        invalid.resize(MAX_SOURCE_BYTES + 1, b' ');
        assert_eq!(
            select(SCOPE, PLAN, PURCHASE, &invalid).unwrap_err(),
            Reject::InvalidSource
        );
    }
}
