use crate::{
    drafts::{Draft, Status},
    material_plan::{BuyRow, canonical_id, required_revision, revision, sku},
};
use mongodb::bson::DateTime;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{collections::HashMap, fmt};
use uuid::Uuid;

const MAX_UNIT_CENTS: u64 = 999_999_999; // Same EUR unit price cap as Sales.
pub(super) const MAX_LINES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Failure {
    Invalid,
    Conflict,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Cents(u64);

impl Cents {
    pub(super) const ZERO: Self = Self(0);

    fn parse(value: &str) -> Option<Self> {
        let (euros, cents) = value.split_once('.')?;
        if euros.is_empty()
            || (euros.len() > 1 && euros.starts_with('0'))
            || cents.len() != 2
            || !euros
                .bytes()
                .chain(cents.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        euros
            .parse::<u64>()
            .ok()?
            .checked_mul(100)?
            .checked_add(cents.parse().ok()?)
            .map(Self)
    }

    pub(super) fn checked_mul(self, pieces: u64) -> Result<Self, Failure> {
        self.0.checked_mul(pieces).map(Self).ok_or(Failure::Invalid)
    }

    pub(super) fn checked_add(self, other: Self) -> Result<Self, Failure> {
        self.0
            .checked_add(other.0)
            .map(Self)
            .ok_or(Failure::Invalid)
    }

    pub(super) fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Cents {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{:02}", self.0 / 100, self.0 % 100)
    }
}

impl Serialize for Cents {
    fn serialize<S: Serializer>(&self, output: S) -> Result<S::Ok, S::Error> {
        output.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Cents {
    fn deserialize<D: Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(input)?)
            .ok_or_else(|| de::Error::custom("expected canonical EUR cents"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
struct UnitPrice(Cents);

impl<'de> Deserialize<'de> for UnitPrice {
    fn deserialize<D: Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        let cents = Cents::deserialize(input)?;
        (cents.0 <= MAX_UNIT_CENTS)
            .then_some(Self(cents))
            .ok_or_else(|| de::Error::custom("EUR unit price exceeds limit"))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Command {
    #[serde(with = "canonical_id")]
    pub command_id: Uuid,
    expected_revision: i32,
    lines: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Choice {
    sku: String,
    #[serde(deserialize_with = "required_revision")]
    revision: Option<String>,
    #[serde(with = "canonical_id")]
    supplier_id: Uuid,
    unit_price: UnitPrice,
}

impl Choice {
    fn key(&self) -> (&str, Option<&str>) {
        (&self.sku, self.revision.as_deref())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Supplier {
    #[serde(with = "canonical_id")]
    pub id: Uuid,
    pub code: String,
    pub name: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum Currency {
    #[serde(rename = "EUR")]
    Eur,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Line {
    #[serde(flatten)]
    original: BuyRow,
    supplier: Supplier,
    unit_price: UnitPrice,
    line_total: Cents,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Terms {
    #[serde(with = "canonical_id")]
    pub proposal_id: Uuid,
    #[serde(with = "canonical_id")]
    pub plan_id: Uuid,
    status: Status,
    pub revision: i32,
    currency: Currency,
    lines: Vec<Line>,
    total: Cents,
}

impl Terms {
    pub(super) fn budget_amount(&self) -> (&'static str, u64, String) {
        let currency = match self.currency {
            Currency::Eur => "EUR",
        };
        (currency, self.total.as_u64(), self.total.to_string())
    }

    pub(super) fn issuance_lines(
        &self,
    ) -> impl ExactSizeIterator<Item = (&BuyRow, &Supplier, Cents, Cents)> {
        self.lines.iter().map(|line| {
            (
                &line.original,
                &line.supplier,
                line.unit_price.0,
                line.line_total,
            )
        })
    }

    pub(super) fn total(&self) -> Cents {
        self.total
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Saved {
    #[serde(with = "canonical_id")]
    command_id: Uuid,
    expected_revision: i32,
    #[serde(with = "canonical_id")]
    pub actor_id: Uuid,
    saved_at: DateTime,
    schema_version: i32,
    pub terms: Terms,
}

impl Saved {
    pub(super) fn has_valid_revision(&self) -> bool {
        self.schema_version == 1
            && self.expected_revision >= 0
            && self.expected_revision.checked_add(1) == Some(self.terms.revision)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Decision {
    Replay(Terms),
    Create(i32),
}

impl Command {
    pub(super) fn normalize(mut self) -> Result<Self, Failure> {
        if self.expected_revision < 0
            || !(1..=MAX_LINES).contains(&self.lines.len())
            || !self
                .lines
                .iter()
                .all(|line| sku(&line.sku) && line.revision.as_deref().is_none_or(revision))
        {
            return Err(Failure::Invalid);
        }
        self.lines.sort_unstable_by(|a, b| a.key().cmp(&b.key()));
        if self
            .lines
            .windows(2)
            .any(|pair| pair[0].key() == pair[1].key())
        {
            return Err(Failure::Invalid);
        }
        Ok(self)
    }

    pub(super) fn decide(&self, latest: i32, saved: Option<Saved>) -> Result<Decision, Failure> {
        if let Some(saved) = saved {
            // The stored snapshot is already the canonical per-line intent.
            // Compare before checking the current revision or resolving suppliers.
            let same = saved.command_id == self.command_id
                && saved.expected_revision == self.expected_revision
                && saved.terms.lines.len() == self.lines.len()
                && saved.terms.lines.iter().zip(&self.lines).all(|(old, new)| {
                    old.original.key() == new.key()
                        && old.supplier.id == new.supplier_id
                        && old.unit_price == new.unit_price
                });
            return if same {
                Ok(Decision::Replay(saved.terms))
            } else {
                Err(Failure::Conflict)
            };
        }
        if latest != self.expected_revision {
            return Err(Failure::Conflict);
        }
        latest
            .checked_add(1)
            .map(Decision::Create)
            .ok_or(Failure::Conflict)
    }

    pub(super) fn bind<'a>(&'a self, draft: &'a Draft) -> Result<BoundCommand<'a>, Failure> {
        if self.lines.len() != draft.plan.buy_rows.len() {
            return Err(Failure::Invalid);
        }
        let lines = draft
            .plan
            .buy_rows
            .iter()
            .zip(&self.lines)
            .map(|(row, choice)| {
                if row.key() != choice.key() {
                    return Err(Failure::Invalid);
                }
                let pieces = row.whole_pieces().ok_or(Failure::Invalid)?;
                Ok((row, choice, pieces))
            })
            .collect::<Result<_, _>>()?;
        Ok(BoundCommand {
            command: self,
            draft,
            lines,
        })
    }
}

pub(super) struct BoundCommand<'a> {
    command: &'a Command,
    draft: &'a Draft,
    lines: Vec<(&'a BuyRow, &'a Choice, u64)>,
}

impl BoundCommand<'_> {
    pub(super) fn supplier_ids(&self) -> impl Iterator<Item = Uuid> {
        self.lines.iter().map(|(_, choice, _)| choice.supplier_id)
    }

    pub(super) fn snapshot(
        self,
        revision: i32,
        suppliers: &HashMap<Uuid, Supplier>,
        actor_id: Uuid,
    ) -> Result<Saved, Failure> {
        let mut lines = Vec::with_capacity(self.lines.len());
        let mut total = Cents::ZERO;
        for (row, choice, pieces) in self.lines {
            let supplier = suppliers
                .get(&choice.supplier_id)
                .filter(|supplier| supplier.id == choice.supplier_id)
                .ok_or(Failure::Unavailable)?;
            let line_total = choice.unit_price.0.checked_mul(pieces)?;
            total = total.checked_add(line_total)?;
            lines.push(Line {
                original: row.clone(),
                supplier: supplier.clone(),
                unit_price: choice.unit_price,
                line_total,
            });
        }
        Ok(Saved {
            command_id: self.command.command_id,
            expected_revision: self.command.expected_revision,
            actor_id,
            saved_at: DateTime::now(),
            schema_version: 1,
            terms: Terms {
                proposal_id: self.draft.proposal_id,
                plan_id: self.draft.plan.plan_id,
                status: Status::Draft,
                revision,
                currency: Currency::Eur,
                lines,
                total,
            },
        })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{
        Scope,
        material_plan::tests::{read, sample},
    };
    use serde_json::{Value, json};

    pub(crate) fn command(value: &Value) -> Result<Command, Failure> {
        serde_json::from_value::<Command>(value.clone())
            .map_err(|_| Failure::Invalid)?
            .normalize()
    }

    pub(crate) fn fixture() -> (Scope, Draft, Value, Supplier) {
        let input = read(&sample()).unwrap();
        let supplier = Supplier {
            id: Uuid::new_v4(),
            code: "SUP-1".into(),
            name: "Supplier One".into(),
        };
        let command = json!({
            "command_id": Uuid::new_v4(),
            "expected_revision": 0,
            "lines": [
                {"sku": "NM-SPINDLE", "revision": "B", "supplier_id": supplier.id, "unit_price": "1.23"},
                {"sku": "NM-MOTOR", "revision": null, "supplier_id": supplier.id, "unit_price": "10.01"},
                {"sku": "NM-SPINDLE", "revision": null, "supplier_id": supplier.id, "unit_price": "9.99"}
            ]
        });
        (input.scope, Draft::new(&input), command, supplier)
    }

    #[test]
    fn cents_preserve_canonical_exact_prices_and_check_arithmetic() {
        for (value, expected) in [("0.00", 0), ("0.07", 7), ("9999999.99", 999_999_999)] {
            let unit: UnitPrice = serde_json::from_value(json!(value)).unwrap();
            assert_eq!(unit.0, Cents(expected));
            assert_eq!(serde_json::to_value(unit).unwrap(), json!(value));
        }
        for invalid in [
            json!(1.01),
            json!("1"),
            json!("1.1"),
            json!("1.234"),
            json!("001.23"),
            json!("+1.23"),
            json!("-1.23"),
            json!("1.23 "),
            json!("１.２３"),
            json!("10000000.00"),
            json!("184467440737095516.16"),
        ] {
            assert!(serde_json::from_value::<UnitPrice>(invalid).is_err());
        }
        let line = Cents(MAX_UNIT_CENTS).checked_mul(1_000_000).unwrap();
        assert_eq!(line.to_string(), "9999999990000.00");
        let total = (0..MAX_LINES)
            .try_fold(Cents(0), |total, _| total.checked_add(line))
            .unwrap();
        assert_eq!(total.to_string(), "319999999680000.00");
        assert_eq!(Cents(u64::MAX).checked_add(Cents(1)), Err(Failure::Invalid));
        assert_eq!(Cents(u64::MAX).checked_mul(2), Err(Failure::Invalid));
    }

    #[test]
    fn command_binds_every_immutable_shortage_and_requires_exact_keys() {
        let (_, draft, body, supplier) = fixture();
        let valid = command(&body).unwrap();
        let terms = valid
            .bind(&draft)
            .unwrap()
            .snapshot(
                1,
                &HashMap::from([(supplier.id, supplier.clone())]),
                Uuid::new_v4(),
            )
            .unwrap()
            .terms;
        assert_eq!(
            serde_json::to_value(terms).unwrap(),
            json!({
                "proposal_id": draft.proposal_id, "plan_id": draft.plan.plan_id,
                "status": "draft", "revision": 1, "currency": "EUR", "total": "162.42",
                "lines": [
                    {"sku":"NM-MOTOR","revision":null,"unit":"EA","pieces":"6",
                     "supplier":supplier,"unit_price":"10.01","line_total":"60.06"},
                    {"sku":"NM-SPINDLE","revision":null,"unit":"EA","pieces":"10",
                     "supplier":supplier,"unit_price":"9.99","line_total":"99.90"},
                    {"sku":"NM-SPINDLE","revision":"B","unit":"EA","pieces":"2",
                     "supplier":supplier,"unit_price":"1.23","line_total":"2.46"}
                ]
            })
        );

        let mut unknown = body.clone();
        unknown["lines"][0]["revision"] = json!("C");
        assert!(matches!(
            command(&unknown).unwrap().bind(&draft),
            Err(Failure::Invalid)
        ));
        let mut omitted = body.clone();
        omitted["lines"].as_array_mut().unwrap().pop();
        assert!(matches!(
            command(&omitted).unwrap().bind(&draft),
            Err(Failure::Invalid)
        ));
        let mut duplicate = body.clone();
        duplicate["lines"][0]["revision"] = Value::Null;
        assert!(matches!(command(&duplicate), Err(Failure::Invalid)));
        let mut missing = body.clone();
        missing["lines"][0]
            .as_object_mut()
            .unwrap()
            .remove("revision");
        assert!(matches!(command(&missing), Err(Failure::Invalid)));
        for key in ["unit", "pieces", "scope", "currency", "line_total"] {
            let mut extra = body.clone();
            extra["lines"][0][key] = json!("not editable");
            assert!(matches!(command(&extra), Err(Failure::Invalid)));
        }
        let mut extra = body.clone();
        extra["total"] = json!("167.13");
        assert!(matches!(command(&extra), Err(Failure::Invalid)));
        let mut zero = sample();
        zero["payload"]["buyRows"][0]["pieces"] = json!("0");
        assert!(read(&zero).is_none());
    }

    #[test]
    fn replay_compares_intent_before_latest_revision_and_conflicts_on_changes() {
        let (_, draft, body, supplier) = fixture();
        let first = command(&body).unwrap();
        let actor = Uuid::new_v4();
        let save = || {
            first
                .bind(&draft)
                .unwrap()
                .snapshot(1, &HashMap::from([(supplier.id, supplier.clone())]), actor)
                .unwrap()
        };
        assert_eq!(first.decide(0, None), Ok(Decision::Create(1)));
        assert_eq!(first.decide(1, None), Err(Failure::Conflict));

        let mut retry = body.clone();
        retry["lines"].as_array_mut().unwrap().reverse();
        assert_eq!(
            command(&retry).unwrap().decide(8, Some(save())),
            Ok(Decision::Replay(save().terms))
        );
        for (key, change) in [
            ("revision", json!("C")),
            ("unit_price", json!("1.24")),
            ("supplier_id", json!(Uuid::new_v4())),
        ] {
            let mut changed = body.clone();
            changed["lines"][0][key] = change;
            assert_eq!(
                command(&changed).unwrap().decide(1, Some(save())),
                Err(Failure::Conflict)
            );
        }
        let mut edit = body.clone();
        edit["expected_revision"] = json!(1);
        assert_eq!(
            command(&edit).unwrap().decide(1, Some(save())),
            Err(Failure::Conflict)
        );
        edit["command_id"] = json!(Uuid::new_v4());
        assert_eq!(
            command(&edit).unwrap().decide(1, None),
            Ok(Decision::Create(2))
        );
        edit["expected_revision"] = json!(i32::MAX);
        assert_eq!(
            command(&edit).unwrap().decide(i32::MAX, None),
            Err(Failure::Conflict)
        );
        for invalid in [json!(-1), json!(0.5), json!(i64::from(i32::MAX) + 1)] {
            edit["expected_revision"] = invalid;
            assert!(matches!(command(&edit), Err(Failure::Invalid)));
        }
    }
}
