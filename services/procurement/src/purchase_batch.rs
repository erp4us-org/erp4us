//! Formation of a local purchase issuance record; no supplier or inventory effects.

use crate::{
    Scope,
    budget_approvals::{self, Record, Review},
    budget_policy::Policy,
    drafts::Draft,
    material_plan::{BuyRow, canonical_id, revision, sku},
    terms::{Cents, Currency, MAX_LINES, Saved, Supplier},
};
use mongodb::bson::DateTime;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Failure {
    InvalidIntent,
    TermsMismatch,
    InvalidSnapshot,
    InvalidLineKey,
    DuplicateLineKey,
    InconsistentSupplier,
    InconsistentTotal,
    Overflow,
    InvalidAllocatedId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IssueFailure {
    Invalid,
    Forbidden,
    NotFound,
    Conflict,
    Unavailable,
}

impl From<budget_approvals::Failure> for IssueFailure {
    fn from(error: budget_approvals::Failure) -> Self {
        match error {
            budget_approvals::Failure::Invalid => Self::Invalid,
            budget_approvals::Failure::Forbidden => Self::Forbidden,
            budget_approvals::Failure::NotFound => Self::NotFound,
            budget_approvals::Failure::Conflict => Self::Conflict,
            budget_approvals::Failure::Unavailable => Self::Unavailable,
        }
    }
}

/// Explicit command identity and reviewed versions. Not evidence of approval or permission.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Intent {
    #[serde(with = "canonical_id")]
    pub command_id: Uuid,
    pub terms_revision: i32,
    #[serde(with = "canonical_id")]
    pub policy_id: Uuid,
    pub policy_revision: i32,
}

impl Intent {
    pub(super) fn validate(self) -> Result<Self, IssueFailure> {
        if self.command_id.is_nil()
            || self.policy_id.is_nil()
            || self.terms_revision < 1
            || self.policy_revision < 1
        {
            return Err(IssueFailure::Invalid);
        }
        Ok(self)
    }

    fn version(self) -> (i32, Uuid, i32) {
        (self.terms_revision, self.policy_id, self.policy_revision)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Origin {
    pub intent: Intent,
    #[serde(with = "canonical_id")]
    pub actor_id: Uuid,
    pub issued_at: DateTime,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Batch {
    #[serde(with = "canonical_id")]
    batch_id: Uuid,
    origin: Origin,
    #[serde(with = "canonical_id")]
    proposal_id: Uuid,
    #[serde(with = "canonical_id")]
    plan_id: Uuid,
    #[serde(with = "canonical_id")]
    source_order_id: Uuid,
    #[serde(with = "canonical_id")]
    source_line_id: Uuid,
    currency: Currency,
    total: Cents,
    orders: Vec<Order>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Order {
    #[serde(with = "canonical_id")]
    order_id: Uuid,
    supplier: Supplier,
    total: Cents,
    lines: Vec<Line>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Line {
    #[serde(with = "canonical_id")]
    line_id: Uuid,
    #[serde(flatten)]
    row: BuyRow,
    unit_price: Cents,
    line_total: Cents,
}

struct Group<'a> {
    supplier: &'a Supplier,
    total: Cents,
    lines: Vec<(&'a BuyRow, Cents, Cents)>,
}

/// A new attempt bound to the latest terms and configured policy. Checking
/// the stored approval prefix is still required before forming a batch.
pub(super) struct Issuance<'a> {
    review: Review<'a>,
    intent: Intent,
    actor_id: Uuid,
}

impl<'a> Issuance<'a> {
    pub(super) fn new(
        scope: Scope,
        saved: &'a Saved,
        policy: Option<&'a Policy>,
        intent: Intent,
        actor_id: Uuid,
    ) -> Result<Self, IssueFailure> {
        let intent = intent.validate()?;
        if actor_id.is_nil() || actor_id == saved.actor_id {
            return Err(IssueFailure::Forbidden);
        }
        let review = Review::new(scope, saved, policy)?;
        if review.version() != intent.version() {
            return Err(IssueFailure::Conflict);
        }
        Ok(Self {
            review,
            intent,
            actor_id,
        })
    }

    pub(super) fn version(&self) -> (i32, Uuid, i32) {
        self.review.version()
    }

    pub(super) fn form(self, draft: &Draft, decisions: &[Record]) -> Result<Batch, IssueFailure> {
        let (saved, approved_total) = self.review.approved_terms(decisions)?;
        Batch::form(
            draft,
            saved,
            Origin {
                intent: self.intent,
                actor_id: self.actor_id,
                issued_at: DateTime::now(),
            },
            approved_total,
            Uuid::new_v4,
        )
        .map_err(|_| IssueFailure::Unavailable)
    }
}

// The wire result is a separate allowlist, never a serialization of the
// internal issuance receipt or its audit data.
#[derive(Debug, Serialize)]
pub(super) struct IssuedBatch {
    batch_id: Uuid,
    plan_id: Uuid,
    proposal_id: Uuid,
    order_id: Uuid,
    line_id: Uuid,
    terms_revision: i32,
    policy_id: Uuid,
    policy_revision: i32,
    state: IssuedState,
    currency: Currency,
    total: Cents,
    issued_at: String,
    purchase_orders: Vec<IssuedOrder>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum IssuedState {
    Issued,
}

#[derive(Debug, Serialize)]
struct IssuedOrder {
    purchase_order_id: Uuid,
    supplier: Supplier,
    total: Cents,
    lines: Vec<IssuedLine>,
}

#[derive(Debug, Serialize)]
struct IssuedLine {
    purchase_line_id: Uuid,
    #[serde(flatten)]
    row: BuyRow,
    unit_price: Cents,
    line_total: Cents,
}

// The receipt source is deliberately not the commercial batch. Keep this
// projection unexposed while receiver authorization is under review.
#[cfg(test)]
#[derive(Debug, Serialize)]
struct ReceiptSource {
    plan_id: Uuid,
    lines: Vec<ReceiptSourceLine>,
}

#[cfg(test)]
#[derive(Debug, Serialize)]
struct ReceiptSourceLine {
    purchase_order_id: Uuid,
    purchase_line_id: Uuid,
    sku: String,
    revision: Option<String>,
    unit: &'static str,
    ordered_pieces: String,
}

impl Batch {
    /// Call only for a new issuance after verifying scope, issuer separation and all
    /// stages against the same policy, saved terms, and whole EUR total. The caller
    /// owns that check and replay; it must not use a client total or re-form a stored batch.
    fn form(
        draft: &Draft,
        saved: &Saved,
        origin: Origin,
        approved_total_cents: u64,
        mut allocate_id: impl FnMut() -> Uuid,
    ) -> Result<Self, Failure> {
        let terms = &saved.terms;
        let plan = &draft.plan;
        if origin.intent.validate().is_err() || origin.actor_id.is_nil() {
            return Err(Failure::InvalidIntent);
        }
        if origin.intent.terms_revision != terms.revision {
            return Err(Failure::TermsMismatch);
        }
        if !saved.has_valid_revision()
            || saved.actor_id.is_nil()
            || terms.plan_id != plan.plan_id
            || terms.proposal_id != draft.proposal_id
            || [plan.plan_id, plan.order_id, plan.line_id, draft.proposal_id]
                .iter()
                .any(Uuid::is_nil)
        {
            return Err(Failure::InvalidSnapshot);
        }
        if terms.total().as_u64() != approved_total_cents {
            return Err(Failure::InconsistentTotal);
        }
        let lines = terms.issuance_lines();
        if !(1..=MAX_LINES).contains(&lines.len()) || lines.len() != plan.buy_rows.len() {
            return Err(Failure::InvalidSnapshot);
        }
        // The saved snapshot is normally sorted. Canonicalize independently of storage order.
        let mut lines: Vec<_> = lines.collect();
        lines.sort_unstable_by_key(|(row, ..)| row.key());
        if !lines.iter().all(|(row, ..)| {
            let (item, rev) = row.key();
            sku(item) && rev.is_none_or(revision)
        }) {
            return Err(Failure::InvalidLineKey);
        }
        if lines
            .windows(2)
            .any(|pair| pair[0].0.key() == pair[1].0.key())
        {
            return Err(Failure::DuplicateLineKey);
        }

        let mut planned: Vec<_> = plan.buy_rows.iter().collect();
        planned.sort_unstable_by_key(|row| row.key());
        let mut groups = BTreeMap::new();
        for ((row, supplier, unit_price, line_total), planned) in lines.into_iter().zip(planned) {
            if row != planned {
                return Err(Failure::InvalidSnapshot);
            }
            let pieces = row.whole_pieces().ok_or(Failure::InvalidSnapshot)?;
            if unit_price
                .checked_mul(pieces)
                .map_err(|_| Failure::Overflow)?
                != line_total
            {
                return Err(Failure::InconsistentTotal);
            }
            if !valid_supplier(supplier) {
                return Err(Failure::InconsistentSupplier);
            }
            let group = groups.entry(supplier.id).or_insert_with(|| Group {
                supplier,
                total: Cents::ZERO,
                lines: Vec::new(),
            });
            if group.supplier != supplier {
                return Err(Failure::InconsistentSupplier);
            }
            group.total = group
                .total
                .checked_add(line_total)
                .map_err(|_| Failure::Overflow)?;
            group.lines.push((row, unit_price, line_total));
        }
        let total = groups.values().try_fold(Cents::ZERO, |total, group| {
            total
                .checked_add(group.total)
                .map_err(|_| Failure::Overflow)
        })?;
        if total != terms.total() {
            return Err(Failure::InconsistentTotal);
        }

        // Allocate only after verifying the whole snapshot. Refuse rather than reuse an ID.
        let mut ids = HashSet::new();
        let mut fresh_id = || {
            let id = allocate_id();
            if id.is_nil() || !ids.insert(id) {
                return Err(Failure::InvalidAllocatedId);
            }
            Ok(id)
        };
        let batch_id = fresh_id()?;
        let mut orders = Vec::with_capacity(groups.len());
        for group in groups.into_values() {
            let order_id = fresh_id()?;
            let lines = group
                .lines
                .into_iter()
                .map(|(row, unit_price, line_total)| {
                    Ok(Line {
                        line_id: fresh_id()?,
                        row: row.clone(),
                        unit_price,
                        line_total,
                    })
                })
                .collect::<Result<_, _>>()?;
            orders.push(Order {
                order_id,
                supplier: group.supplier.clone(),
                total: group.total,
                lines,
            });
        }
        Ok(Self {
            batch_id,
            origin,
            proposal_id: terms.proposal_id,
            plan_id: terms.plan_id,
            source_order_id: plan.order_id,
            source_line_id: plan.line_id,
            currency: Currency::Eur,
            total,
            orders,
        })
    }

    #[cfg(test)]
    fn origin(&self) -> Origin {
        self.origin
    }

    #[cfg(test)]
    fn batch_id(&self) -> Uuid {
        self.batch_id
    }

    pub(super) fn plan_id(&self) -> Uuid {
        self.plan_id
    }

    pub(super) fn replay(self, intent: Intent, actor_id: Uuid) -> Result<Self, IssueFailure> {
        if self.origin.intent != intent || self.origin.actor_id != actor_id {
            return Err(IssueFailure::Conflict);
        }
        Ok(self)
    }

    #[cfg(test)]
    fn receipt_source(&self) -> Result<ReceiptSource, IssueFailure> {
        if self.plan_id.is_nil() || self.batch_id.is_nil() {
            return Err(IssueFailure::Unavailable);
        }

        let mut lines = Vec::new();
        let mut ids = HashSet::from([self.batch_id]);
        let mut items = HashSet::new();
        for order in &self.orders {
            if order.order_id.is_nil() || !ids.insert(order.order_id) || order.lines.is_empty() {
                return Err(IssueFailure::Unavailable);
            }
            for line in &order.lines {
                let (item, pin) = line.row.key();
                if lines.len() == MAX_LINES
                    || line.line_id.is_nil()
                    || !ids.insert(line.line_id)
                    || !sku(item)
                    || pin.is_some_and(|value| !revision(value))
                    || !items.insert((item, pin))
                {
                    return Err(IssueFailure::Unavailable);
                }
                let pieces = line.row.whole_pieces().ok_or(IssueFailure::Unavailable)?;
                lines.push(ReceiptSourceLine {
                    purchase_order_id: order.order_id,
                    purchase_line_id: line.line_id,
                    sku: item.into(),
                    revision: pin.map(str::to_owned),
                    unit: "EA",
                    ordered_pieces: pieces.to_string(),
                });
            }
        }
        if lines.is_empty() {
            return Err(IssueFailure::Unavailable);
        }
        lines.sort_unstable_by_key(|line| (line.purchase_order_id, line.purchase_line_id));
        Ok(ReceiptSource {
            plan_id: self.plan_id,
            lines,
        })
    }
}

impl TryFrom<Batch> for IssuedBatch {
    type Error = IssueFailure;

    fn try_from(batch: Batch) -> Result<Self, Self::Error> {
        Ok(Self {
            batch_id: batch.batch_id,
            plan_id: batch.plan_id,
            proposal_id: batch.proposal_id,
            order_id: batch.source_order_id,
            line_id: batch.source_line_id,
            terms_revision: batch.origin.intent.terms_revision,
            policy_id: batch.origin.intent.policy_id,
            policy_revision: batch.origin.intent.policy_revision,
            state: IssuedState::Issued,
            currency: batch.currency,
            total: batch.total,
            issued_at: batch
                .origin
                .issued_at
                .try_to_rfc3339_string()
                .map_err(|_| IssueFailure::Unavailable)?,
            purchase_orders: batch
                .orders
                .into_iter()
                .map(|order| IssuedOrder {
                    purchase_order_id: order.order_id,
                    supplier: order.supplier,
                    total: order.total,
                    lines: order
                        .lines
                        .into_iter()
                        .map(|line| IssuedLine {
                            purchase_line_id: line.line_id,
                            row: line.row,
                            unit_price: line.unit_price,
                            line_total: line.line_total,
                        })
                        .collect(),
                })
                .collect(),
        })
    }
}

// Check the historical snapshot shape, not whether the supplier is still active.
fn valid_supplier(supplier: &Supplier) -> bool {
    !supplier.id.is_nil()
        && (1..=32).contains(&supplier.code.len())
        && supplier
            .code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-')
        && (1..=120).contains(&supplier.name.chars().count())
        && !supplier.name.trim().is_empty()
        && !supplier.name.chars().any(char::is_control)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{
        budget_approvals::tests::{APPROVER, policy, stage},
        budget_policy::JsonObject,
        material_plan::tests::read,
        terms::tests::command,
    };
    use serde_json::{Value, json};
    use std::collections::HashMap;

    const APPROVED: u64 = 188_240;
    pub(crate) const ISSUER: Uuid = Uuid::from_u128(6);
    const FIXTURE: &str =
        include_str!("../../../contracts/examples/nats/inventory-material-plan-netted-v1.json");
    const ORIGIN: Origin = Origin {
        intent: Intent {
            command_id: Uuid::from_u128(1),
            terms_revision: 1,
            policy_id: Uuid::from_u128(2),
            policy_revision: 1,
        },
        actor_id: Uuid::from_u128(3),
        issued_at: DateTime::from_millis(1_774_000_000_000),
    };

    fn fixture(split: bool) -> (Draft, Saved, [Supplier; 2]) {
        let draft = Draft::new(&read(&serde_json::from_str(FIXTURE).unwrap()).unwrap());
        let suppliers = [9, 8].map(|n| Supplier {
            id: Uuid::from_u128(n),
            code: format!("SUP-{n}"),
            name: "Historic supplier name".into(),
        });
        let prices = ["30.10", "149.95", "12.00", "7.35", "45.20"];
        let lines: Vec<_> = draft
            .plan
            .buy_rows
            .iter()
            .zip(prices)
            .enumerate()
            .map(|(index, (row, price))| {
                let (sku, revision) = row.key();
                json!({
                    "sku": sku, "revision": revision, "unit_price": price,
                    "supplier_id": suppliers[usize::from(split && index % 2 == 1)].id,
                })
            })
            .collect();
        let verified = suppliers
            .iter()
            .cloned()
            .map(|s| (s.id, s))
            .collect::<HashMap<_, _>>();
        let terms = command(&json!({
            "command_id": Uuid::from_u128(4), "expected_revision": 0, "lines": lines,
        }))
        .unwrap()
        .bind(&draft)
        .unwrap()
        .snapshot(1, &verified, Uuid::from_u128(5))
        .unwrap();
        (draft, terms, suppliers)
    }

    fn ids() -> impl Iterator<Item = Uuid> {
        (100..).map(Uuid::from_u128)
    }

    pub(crate) fn approval_fixture() -> (Scope, Draft, Saved, Policy, Intent, [Record; 2]) {
        let scope = read(&serde_json::from_str(FIXTURE).unwrap()).unwrap().scope;
        let (draft, saved, _) = fixture(true);
        let policy = policy();
        let review = Review::new(scope, &saved, Some(&policy)).unwrap();
        let (terms_revision, policy_id, policy_revision) = review.version();
        let intent = Intent {
            command_id: Uuid::from_u128(7),
            terms_revision,
            policy_id,
            policy_revision,
        };
        let first = review
            .decide(&[], &stage(&review, "department"), APPROVER)
            .unwrap();
        let second = review
            .decide(
                std::slice::from_ref(&first),
                &stage(&review, "budget"),
                Uuid::from_u128(10),
            )
            .unwrap();
        (scope, draft, saved, policy, intent, [first, second])
    }

    #[test]
    fn receipt_source_exposes_sorted_pinned_quantities_without_commercial_or_actor_data() {
        let (draft, terms, _) = fixture(true);
        let mut ids = (100..110).rev().map(Uuid::from_u128);
        let issued = Batch::form(&draft, &terms, ORIGIN, APPROVED, || ids.next().unwrap()).unwrap();
        let source = issued.receipt_source().unwrap();
        assert_eq!(source.lines.len(), 5);
        assert!(source.lines.windows(2).all(|pair| {
            (pair[0].purchase_order_id, pair[0].purchase_line_id)
                < (pair[1].purchase_order_id, pair[1].purchase_line_id)
        }));
        let expected: Vec<_> = source
            .lines
            .iter()
            .map(|line| {
                let order = issued
                    .orders
                    .iter()
                    .find(|order| order.order_id == line.purchase_order_id)
                    .unwrap();
                let original = order
                    .lines
                    .iter()
                    .find(|row| row.line_id == line.purchase_line_id)
                    .unwrap();
                let (item, revision) = original.row.key();
                json!({
                    "purchase_order_id": order.order_id, "purchase_line_id": original.line_id,
                    "sku": item, "revision": revision, "unit": "EA",
                    "ordered_pieces": original.row.whole_pieces().unwrap().to_string(),
                })
            })
            .collect();
        assert_eq!(
            serde_json::to_value(&source).unwrap(),
            json!({"plan_id": draft.plan.plan_id, "lines": expected})
        );
        assert_eq!(
            source
                .lines
                .iter()
                .find(|line| line.sku == "NM-ENDPLATE")
                .map(|line| line.ordered_pieces.as_str()),
            Some("12")
        );
    }

    #[test]
    fn receipt_source_fails_closed_on_missing_or_ambiguous_issued_lines() {
        let (draft, terms, _) = fixture(true);
        let mut ids = ids();
        let mut issued =
            Batch::form(&draft, &terms, ORIGIN, APPROVED, || ids.next().unwrap()).unwrap();
        issued.orders[1].lines[0].line_id = issued.orders[0].lines[0].line_id;
        assert_eq!(
            issued.receipt_source().unwrap_err(),
            IssueFailure::Unavailable
        );
        issued.orders.clear();
        assert_eq!(
            issued.receipt_source().unwrap_err(),
            IssueFailure::Unavailable
        );
    }

    #[test]
    fn issue_command_accepts_only_exact_canonical_intent() {
        let original = serde_json::to_value(ORIGIN.intent).unwrap();
        let parse = |value: Value| {
            let JsonObject(intent) = serde_json::from_value::<JsonObject<Intent>>(value)
                .map_err(|_| IssueFailure::Invalid)?;
            intent.validate()
        };
        assert_eq!(parse(original.clone()), Ok(ORIGIN.intent));
        for field in original.as_object().unwrap().keys() {
            let mut missing = original.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert_eq!(parse(missing), Err(IssueFailure::Invalid));
        }
        for (field, invalid) in [
            ("command_id", json!(Uuid::nil())),
            ("policy_id", json!("00000000-0000-0000-0000-00000000000A")),
            ("policy_revision", json!("1")),
            ("terms_revision", json!(0)),
            ("actor_id", json!(ISSUER)),
            ("supplier_id", json!(Uuid::new_v4())),
            ("lines", json!([])),
            ("total", json!("1.00")),
        ] {
            let mut changed = original.clone();
            changed[field] = invalid;
            assert_eq!(parse(changed), Err(IssueFailure::Invalid), "{field}");
        }
        assert_eq!(parse(json!([])), Err(IssueFailure::Invalid));
        assert!(
            serde_json::from_str::<JsonObject<Intent>>(&original.to_string().replacen(
                '{',
                r#"{"policy_revision":2,"#,
                1
            ))
            .is_err()
        );
    }

    #[test]
    fn issuance_requires_complete_untampered_approval_for_the_whole_split_draft() {
        let (scope, draft, saved, policy, intent, mut approvals) = approval_fixture();
        let attempt = || Issuance::new(scope, &saved, Some(&policy), intent, ISSUER).unwrap();
        // Each supplier is below the second threshold. That does not exempt
        // either from the full-draft second approval.
        assert_eq!(
            attempt().form(&draft, &approvals[..1]),
            Err(IssueFailure::Conflict)
        );
        let batch = attempt().form(&draft, &approvals).unwrap();
        assert_eq!(batch.total.to_string(), "1882.40");
        assert_eq!(batch.orders.len(), 2);
        approvals.swap(0, 1);
        assert_eq!(
            attempt().form(&draft, &approvals),
            Err(IssueFailure::Unavailable)
        );
        approvals.swap(0, 1);
        let mut altered = serde_json::to_value(&approvals[1]).unwrap();
        altered["originalResult"]["total"] = json!("1882.39");
        approvals[1] = serde_json::from_value(altered).unwrap();
        assert_eq!(
            attempt().form(&draft, &approvals),
            Err(IssueFailure::Unavailable)
        );
    }

    #[test]
    fn issuance_rejects_preparer_missing_policy_foreign_scope_and_stale_versions() {
        let (scope, draft, saved, policy, intent, approvals) = approval_fixture();
        for actor in [saved.actor_id, Uuid::nil()] {
            assert!(matches!(
                Issuance::new(scope, &saved, Some(&policy), intent, actor),
                Err(IssueFailure::Forbidden)
            ));
        }
        assert!(matches!(
            Issuance::new(scope, &saved, None, intent, ISSUER),
            Err(IssueFailure::Unavailable)
        ));
        let foreign = Scope {
            organization_id: Uuid::new_v4(),
            ..scope
        };
        assert!(matches!(
            Issuance::new(foreign, &saved, Some(&policy), intent, ISSUER),
            Err(IssueFailure::Unavailable)
        ));
        for stale in [
            Intent {
                terms_revision: 2,
                ..intent
            },
            Intent {
                policy_revision: 2,
                ..intent
            },
            Intent {
                policy_id: Uuid::new_v4(),
                ..intent
            },
        ] {
            assert!(matches!(
                Issuance::new(scope, &saved, Some(&policy), stale, ISSUER),
                Err(IssueFailure::Conflict)
            ));
        }
        let mut config: Value = serde_json::from_slice(include_bytes!(
            "../../../contracts/examples/procurement/budget-policy-v1.json"
        ))
        .unwrap();
        config["stages"][1]["minimum_amount"] = json!("1999.00");
        let edited = Policy::parse(Some(&serde_json::to_vec(&config).unwrap())).unwrap();
        let attempt = Issuance::new(scope, &saved, Some(&edited), intent, ISSUER).unwrap();
        // A policy edited in place cannot borrow even the first-stage approval.
        assert_eq!(
            attempt.form(&draft, &approvals[..1]),
            Err(IssueFailure::Unavailable)
        );
    }

    #[test]
    fn public_batch_returns_only_contract_fields_and_historical_source_references() {
        let (draft, saved, suppliers) = fixture(false);
        let mut ids = ids();
        let batch = Batch::form(&draft, &saved, ORIGIN, APPROVED, || ids.next().unwrap()).unwrap();
        let expected_lines = [
            ("NM-ENDPLATE", "12", "30.10", "361.20"),
            ("NM-MOTOR", "6", "149.95", "899.70"),
            ("NM-RAIL", "8", "12.00", "96.00"),
            ("NM-SENSOR", "10", "7.35", "73.50"),
            ("NM-SPINDLE", "10", "45.20", "452.00"),
        ]
        .into_iter()
        .zip(102..)
        .map(|((sku, pieces, price, total), id)| {
            json!({
                "purchase_line_id": Uuid::from_u128(id), "sku": sku, "revision": null,
                "unit": "EA", "pieces": pieces, "unit_price": price, "line_total": total,
            })
        })
        .collect::<Vec<_>>();
        assert_eq!(
            serde_json::to_value(IssuedBatch::try_from(batch).unwrap()).unwrap(),
            json!({
                "batch_id": Uuid::from_u128(100), "proposal_id": draft.proposal_id,
                "plan_id": draft.plan.plan_id, "order_id": draft.plan.order_id,
                "line_id": draft.plan.line_id, "state": "issued", "currency": "EUR",
                "terms_revision": 1, "policy_id": ORIGIN.intent.policy_id, "policy_revision": 1,
                "total": "1882.40", "issued_at": "2026-03-20T09:46:40Z",
                "purchase_orders": [{
                    "purchase_order_id": Uuid::from_u128(101), "supplier": suppliers[0],
                    "total": "1882.40", "lines": expected_lines,
                }],
            })
        );
    }

    #[test]
    fn one_supplier_retains_five_exact_eur_lines_and_source_references() {
        let (draft, saved, suppliers) = fixture(false);
        let mut ids = ids();
        let batch = Batch::form(&draft, &saved, ORIGIN, APPROVED, || ids.next().unwrap()).unwrap();
        assert_eq!(batch.origin(), ORIGIN);
        assert_eq!(batch.batch_id(), Uuid::from_u128(100));
        assert_eq!(batch.plan_id(), draft.plan.plan_id);
        assert_eq!(batch.proposal_id, draft.proposal_id);
        assert_eq!(batch.source_order_id, draft.plan.order_id);
        assert_eq!(batch.source_line_id, draft.plan.line_id);
        assert_eq!(batch.currency, Currency::Eur);
        assert_eq!(batch.total.to_string(), "1882.40");
        let [order] = batch.orders.as_slice() else {
            panic!("expected one purchase order");
        };
        assert_eq!(order.order_id, Uuid::from_u128(101));
        assert_eq!(order.supplier, suppliers[0]);
        assert_eq!(order.total, batch.total);
        let actual = order.lines.iter().map(|line| {
            (
                line.row.key().0,
                line.row.whole_pieces().unwrap(),
                line.unit_price.to_string(),
                line.line_total.to_string(),
            )
        });
        let expected = [
            ("NM-ENDPLATE", 12, "30.10", "361.20"),
            ("NM-MOTOR", 6, "149.95", "899.70"),
            ("NM-RAIL", 8, "12.00", "96.00"),
            ("NM-SENSOR", 10, "7.35", "73.50"),
            ("NM-SPINDLE", 10, "45.20", "452.00"),
        ];
        assert!(actual.eq(
            expected.map(|(sku, pieces, unit, line)| {
                (sku, pieces, unit.to_owned(), line.to_owned())
            })
        ));
        assert!(
            order
                .lines
                .iter()
                .map(|line| line.line_id)
                .eq((102..107).map(Uuid::from_u128))
        );
        assert_eq!(ids.next(), Some(Uuid::from_u128(107)));
    }

    #[test]
    fn split_groups_use_supplier_id_order_and_reconcile_the_whole_approved_total() {
        let (mut draft, saved, suppliers) = fixture(true);
        let mut shuffled = serde_json::to_value(&saved).unwrap();
        shuffled["terms"]["lines"].as_array_mut().unwrap().reverse();
        let shuffled = serde_json::from_value(shuffled).unwrap();
        let form = |draft: &Draft, terms: &Saved| {
            let mut ids = ids();
            Batch::form(draft, terms, ORIGIN, APPROVED, || ids.next().unwrap()).unwrap()
        };
        let canonical = form(&draft, &saved);
        draft.plan.buy_rows.reverse();
        let batch = form(&draft, &shuffled);
        assert_eq!(batch, canonical);
        assert_eq!(batch.total.to_string(), "1882.40");
        let [first, second] = batch.orders.as_slice() else {
            panic!("expected two purchase orders");
        };
        for (order, supplier, total, rows) in [
            (
                first,
                &suppliers[1],
                "973.20",
                &["NM-MOTOR", "NM-SENSOR"][..],
            ),
            (
                second,
                &suppliers[0],
                "909.20",
                &["NM-ENDPLATE", "NM-RAIL", "NM-SPINDLE"][..],
            ),
        ] {
            assert_eq!(&order.supplier, supplier);
            assert_eq!(order.total.to_string(), total);
            assert!(
                order
                    .lines
                    .iter()
                    .map(|line| line.row.key().0)
                    .eq(rows.iter().copied())
            );
        }
        assert_eq!(first.total.checked_add(second.total), Ok(batch.total));
        assert_eq!(
            Batch::form(&draft, &saved, ORIGIN, first.total.as_u64(), || panic!(
                "allocated ID"
            )),
            Err(Failure::InconsistentTotal)
        );
    }

    #[test]
    fn inconsistent_snapshots_fail_before_allocating_ids() {
        let (draft, saved, _) = fixture(false);
        let original = serde_json::to_value(saved).unwrap();
        let check = |snapshot: Value, expected| {
            let terms = serde_json::from_value(snapshot).unwrap();
            assert_eq!(
                Batch::form(&draft, &terms, ORIGIN, APPROVED, || panic!("allocated ID")),
                Err(expected)
            );
        };

        let mut duplicate = original.clone();
        duplicate["terms"]["lines"][1]["sku"] = duplicate["terms"]["lines"][0]["sku"].clone();
        check(duplicate, Failure::DuplicateLineKey);
        for (field, value, failure) in [
            ("sku", json!(""), Failure::InvalidLineKey),
            ("revision", json!("?"), Failure::InvalidLineKey),
            ("pieces", json!("13"), Failure::InvalidSnapshot),
            ("pieces", json!("12.0"), Failure::InvalidSnapshot),
            ("line_total", json!("361.19"), Failure::InconsistentTotal),
            ("unit_price", json!("30.11"), Failure::InconsistentTotal),
        ] {
            let mut changed = original.clone();
            changed["terms"]["lines"][0][field] = value;
            check(changed, failure);
        }
        let mut supplier = original.clone();
        supplier["terms"]["lines"][0]["supplier"]["name"] = json!("A different historic name");
        check(supplier, Failure::InconsistentSupplier);
        let mut malformed = original.clone();
        malformed["terms"]["lines"][0]["supplier"]["code"] = json!("");
        check(malformed, Failure::InconsistentSupplier);
        let mut missing = original.clone();
        missing["terms"]["lines"].as_array_mut().unwrap().pop();
        check(missing, Failure::InvalidSnapshot);
        let mut wrong_proposal = original.clone();
        wrong_proposal["terms"]["proposal_id"] = json!(Uuid::new_v4());
        check(wrong_proposal, Failure::InvalidSnapshot);
        let mut wrong_revision = original.clone();
        wrong_revision["schemaVersion"] = json!(2);
        check(wrong_revision, Failure::InvalidSnapshot);
        let mut wrong_total = original.clone();
        wrong_total["terms"]["total"] = json!("1882.41");
        check(wrong_total, Failure::InconsistentTotal);
        // Agreement with the saved/approved total is not enough: all lines must add up to it.
        let mut misadded = original.clone();
        misadded["terms"]["lines"][0]["unit_price"] = json!("30.11");
        misadded["terms"]["lines"][0]["line_total"] = json!("361.32");
        check(misadded, Failure::InconsistentTotal);

        for (field, value) in [
            ("unit_price", json!("184467440737095516.16")),
            ("unit_price", json!("10000000.00")),
            ("unit", json!("KG")),
        ] {
            let mut invalid = original.clone();
            invalid["terms"]["lines"][0][field] = value;
            assert!(serde_json::from_value::<Saved>(invalid).is_err());
        }
    }

    #[test]
    fn rejects_invalid_quantities_even_if_the_stored_plan_row_also_contains_them() {
        let (mut draft, saved, _) = fixture(false);
        let original = serde_json::to_value(saved).unwrap();
        for pieces in ["0", "012", "12.5", "1000001", "18446744073709551616"] {
            let mut corrupted = original.clone();
            corrupted["terms"]["lines"][0]["pieces"] = json!(pieces);
            draft.plan.buy_rows[0] =
                serde_json::from_value(corrupted["terms"]["lines"][0].clone()).unwrap();
            assert_eq!(
                Batch::form(
                    &draft,
                    &serde_json::from_value(corrupted).unwrap(),
                    ORIGIN,
                    APPROVED,
                    || panic!("allocated ID"),
                ),
                Err(Failure::InvalidSnapshot)
            );
        }
    }

    #[test]
    fn rejects_stale_terms_and_nil_or_colliding_ids() {
        let (draft, saved, _) = fixture(false);
        let mut stale = ORIGIN;
        stale.intent.terms_revision = 2;
        assert_eq!(
            Batch::form(&draft, &saved, stale, APPROVED, || panic!("allocated ID")),
            Err(Failure::TermsMismatch)
        );
        let mut nil = ORIGIN;
        nil.intent.command_id = Uuid::nil();
        assert_eq!(
            Batch::form(&draft, &saved, nil, APPROVED, || panic!("allocated ID")),
            Err(Failure::InvalidIntent)
        );
        for bad in [Uuid::nil(), Uuid::from_u128(100)] {
            let mut ids = [Uuid::from_u128(100), bad].into_iter();
            assert_eq!(
                Batch::form(&draft, &saved, ORIGIN, APPROVED, || ids.next().unwrap()),
                Err(Failure::InvalidAllocatedId)
            );
        }
    }
}
