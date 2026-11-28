//! Amount-based stages from a trusted deployment policy. No I/O or authorization.

use crate::{Scope, material_plan::canonical_id};
use serde::{Deserialize, Deserializer, Serialize, de};
use std::{fmt, marker::PhantomData};
use uuid::Uuid;

const CURRENCY: &str = "EUR";
pub(super) const MAX_STAGES: usize = 8;
const MAX_KEY_BYTES: usize = 32;
const MAX_THRESHOLD_EURO_DIGITS: usize = 15;

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Failure {
    MissingPolicy,
    InvalidPolicy,
    ScopeMismatch,
    CurrencyMismatch,
    InvalidPrefix,
    NotNextStage,
    InvalidActor,
    SelfApproval,
    RepeatedApprover,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema_version: u32,
    #[serde(with = "canonical_id")]
    tenant_id: Uuid,
    #[serde(with = "canonical_id")]
    organization_id: Uuid,
    #[serde(with = "canonical_id")]
    policy_id: Uuid,
    revision: i32,
    currency: String,
    require_distinct_approvers: bool,
    stages: Vec<JsonObject<StageDocument>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StageDocument {
    key: String,
    minimum_amount: String,
}

// Derived struct deserializers also accept positional arrays. Require objects
// without collecting into a JSON map first (which would erase duplicate fields).
pub(super) struct JsonObject<T>(pub T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for JsonObject<T> {
    fn deserialize<D: Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> de::Visitor<'de> for ObjectVisitor<T> {
            type Value = JsonObject<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(JsonObject)
            }
        }

        input.deserialize_map(ObjectVisitor(PhantomData))
    }
}

#[derive(Debug)]
pub(super) struct Policy {
    tenant_id: Uuid,
    organization_id: Uuid,
    id: Uuid,
    revision: i32,
    require_distinct_approvers: bool,
    stages: Vec<Stage>,
}

#[derive(Debug)]
pub(super) struct Stage {
    key: String,
    permission: String,
    minimum_cents: u64,
}

/// Canonical policy content, independent of JSON formatting. Scope and policy
/// identity are stored separately with the decision; include even stages that
/// do not apply to its total so a reused revision cannot count old approvals.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct PolicySnapshot {
    schema_version: i32,
    currency: String,
    require_distinct_approvers: bool,
    ordered_thresholds_in_cents: Vec<(String, u64)>,
}

impl Stage {
    pub(super) fn key(&self) -> &str {
        &self.key
    }

    /// The controller must check this exact grant in the verified scope.
    pub(super) fn permission(&self) -> &str {
        &self.permission
    }
}

#[derive(Debug)]
pub(super) struct Requirements<'a> {
    policy_id: Uuid,
    policy_revision: i32,
    require_distinct_approvers: bool,
    stages: &'a [Stage],
}

pub(super) struct CompletedStage<'a> {
    pub(super) key: &'a str,
    pub(super) actor_id: Uuid,
}

impl Policy {
    pub(super) fn snapshot(&self) -> PolicySnapshot {
        PolicySnapshot {
            schema_version: 1,
            currency: CURRENCY.to_owned(),
            require_distinct_approvers: self.require_distinct_approvers,
            ordered_thresholds_in_cents: self
                .stages
                .iter()
                .map(|stage| (stage.key.clone(), stage.minimum_cents))
                .collect(),
        }
    }

    /// Parse only the operator's configured document. Absence never supplies a default.
    pub(super) fn parse(input: Option<&[u8]>) -> Result<Self, Failure> {
        let input = input.ok_or(Failure::MissingPolicy)?;
        let JsonObject(document): JsonObject<Document> =
            serde_json::from_slice(input).map_err(|_| Failure::InvalidPolicy)?;
        if document.schema_version != 1
            || document.revision < 1
            || document.currency != CURRENCY
            || !(1..=MAX_STAGES).contains(&document.stages.len())
        {
            return Err(Failure::InvalidPolicy);
        }

        let mut stages: Vec<Stage> = Vec::with_capacity(document.stages.len());
        for JsonObject(StageDocument {
            key,
            minimum_amount,
        }) in document.stages
        {
            let minimum_cents = threshold_cents(&minimum_amount).ok_or(Failure::InvalidPolicy)?;
            let permission = stage_permission(&key).ok_or(Failure::InvalidPolicy)?;
            if stages.iter().any(|stage| stage.key == key)
                || match stages.last() {
                    None => minimum_cents != 0,
                    Some(previous) => minimum_cents <= previous.minimum_cents,
                }
            {
                return Err(Failure::InvalidPolicy);
            }
            stages.push(Stage {
                permission,
                key,
                minimum_cents,
            });
        }

        Ok(Self {
            tenant_id: document.tenant_id,
            organization_id: document.organization_id,
            id: document.policy_id,
            revision: document.revision,
            require_distinct_approvers: document.require_distinct_approvers,
            stages,
        })
    }

    /// Resolve against the verified scope and the entire persisted terms total in cents.
    pub(super) fn requirements(
        &self,
        scope: Scope,
        currency: &str,
        total_cents: u64,
    ) -> Result<Requirements<'_>, Failure> {
        if scope.tenant_id != self.tenant_id || scope.organization_id != self.organization_id {
            return Err(Failure::ScopeMismatch);
        }
        if currency != CURRENCY {
            return Err(Failure::CurrencyMismatch);
        }
        let count = self
            .stages
            .partition_point(|stage| stage.minimum_cents <= total_cents);
        Ok(Requirements {
            policy_id: self.id,
            policy_revision: self.revision,
            require_distinct_approvers: self.require_distinct_approvers,
            stages: &self.stages[..count],
        })
    }
}

impl Requirements<'_> {
    pub(super) fn policy_id(&self) -> Uuid {
        self.policy_id
    }

    pub(super) fn policy_revision(&self) -> i32 {
        self.policy_revision
    }

    pub(super) fn stages(&self) -> &[Stage] {
        self.stages
    }

    /// `Ok(None)` means all required stages are complete. The caller must pass
    /// only decisions bound to the same scope, terms and policy snapshot.
    pub(super) fn next_stage(
        &self,
        terms_editor: Uuid,
        completed: &[CompletedStage<'_>],
    ) -> Result<Option<&Stage>, Failure> {
        if terms_editor.is_nil() {
            return Err(Failure::InvalidActor);
        }
        if completed.len() > self.stages.len() {
            return Err(Failure::InvalidPrefix);
        }
        for (index, (stage, decision)) in self.stages.iter().zip(completed).enumerate() {
            if decision.key != stage.key() {
                return Err(Failure::InvalidPrefix);
            }
            self.validate_actor(terms_editor, &completed[..index], decision.actor_id)?;
        }
        Ok(self.stages.get(completed.len()))
    }

    /// Check that a decision with the verified actor could extend this prefix.
    /// This does not check the actor's token or grant, write a decision, or handle replay.
    pub(super) fn validate_next(
        &self,
        terms_editor: Uuid,
        completed: &[CompletedStage<'_>],
        candidate: CompletedStage<'_>,
    ) -> Result<&Stage, Failure> {
        let next = self
            .next_stage(terms_editor, completed)?
            .filter(|stage| stage.key() == candidate.key)
            .ok_or(Failure::NotNextStage)?;
        self.validate_actor(terms_editor, completed, candidate.actor_id)?;
        Ok(next)
    }

    fn validate_actor(
        &self,
        terms_editor: Uuid,
        preceding: &[CompletedStage<'_>],
        actor_id: Uuid,
    ) -> Result<(), Failure> {
        if actor_id.is_nil() {
            return Err(Failure::InvalidActor);
        }
        if terms_editor == actor_id {
            return Err(Failure::SelfApproval);
        }
        if self.require_distinct_approvers
            && preceding.iter().any(|stage| stage.actor_id == actor_id)
        {
            return Err(Failure::RepeatedApprover);
        }
        Ok(())
    }
}

pub(super) fn stage_permission(key: &str) -> Option<String> {
    let valid = key.len() <= MAX_KEY_BYTES
        && key.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && key
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    valid.then(|| format!("procurement.approval.{key}"))
}

fn threshold_cents(value: &str) -> Option<u64> {
    let (euros, cents) = value.split_once('.')?;
    if !(1..=MAX_THRESHOLD_EURO_DIGITS).contains(&euros.len())
        || (euros.len() > 1 && euros.starts_with('0'))
        || cents.len() != 2
        || !euros
            .bytes()
            .chain(cents.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    // At most 17 decimal digits after conversion, well within u64.
    Some(euros.parse::<u64>().ok()? * 100 + cents.parse::<u64>().ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const EXAMPLE: &[u8] =
        include_bytes!("../../../contracts/examples/procurement/budget-policy-v1.json");
    const EDITOR: Uuid = Uuid::from_u128(1);
    const FIRST: Uuid = Uuid::from_u128(2);
    const SECOND: Uuid = Uuid::from_u128(3);

    fn scope() -> Scope {
        Scope {
            tenant_id: Uuid::parse_str("ddcae139-c1e8-47d2-8b48-349cb6f30ce5").unwrap(),
            organization_id: Uuid::parse_str("3e793fb5-75a7-45f6-8f66-45a58fcec674").unwrap(),
        }
    }

    fn example() -> Value {
        serde_json::from_slice(EXAMPLE).unwrap()
    }

    fn parse(value: &Value) -> Result<Policy, Failure> {
        Policy::parse(Some(&serde_json::to_vec(value).unwrap()))
    }

    fn decision(key: &str, actor_id: Uuid) -> CompletedStage<'_> {
        CompletedStage { key, actor_id }
    }

    #[test]
    fn fixture_requires_inclusive_cumulative_stages_and_exact_grants() {
        let policy = Policy::parse(Some(EXAMPLE)).unwrap();
        for (total_cents, count) in [(0, 1), (149_999, 1), (150_000, 2), (188_240, 2)] {
            let required = policy.requirements(scope(), "EUR", total_cents).unwrap();
            assert_eq!(
                required.policy_id(),
                Uuid::parse_str("b3c1837d-138f-4e22-9b6a-c450aced37bc").unwrap()
            );
            assert_eq!(required.policy_revision(), 1);
            assert_eq!(
                required
                    .stages()
                    .iter()
                    .map(|stage| (stage.key(), stage.permission()))
                    .collect::<Vec<_>>(),
                [
                    ("department", "procurement.approval.department"),
                    ("budget", "procurement.approval.budget"),
                ][..count]
            );
        }
    }

    #[test]
    fn rejects_absent_malformed_and_incompatible_documents() {
        assert_eq!(Policy::parse(None).unwrap_err(), Failure::MissingPolicy);
        for bytes in [b"".as_slice(), b"{", b"null", b"[]"] {
            assert_eq!(
                Policy::parse(Some(bytes)).unwrap_err(),
                Failure::InvalidPolicy
            );
        }
        let duplicate = String::from_utf8(EXAMPLE.to_vec()).unwrap().replacen(
            '{',
            r#"{"schema_version":1,"#,
            1,
        );
        assert_eq!(
            Policy::parse(Some(duplicate.as_bytes())).unwrap_err(),
            Failure::InvalidPolicy
        );

        let valid = example();
        let positional = json!([
            valid["schema_version"],
            valid["tenant_id"],
            valid["organization_id"],
            valid["policy_id"],
            valid["revision"],
            valid["currency"],
            valid["require_distinct_approvers"],
            valid["stages"],
        ]);
        assert_eq!(parse(&positional).unwrap_err(), Failure::InvalidPolicy);
        let mut positional_stage = valid.clone();
        positional_stage["stages"][0] = json!(["department", "0.00"]);
        assert_eq!(
            parse(&positional_stage).unwrap_err(),
            Failure::InvalidPolicy
        );
        let too_many = (0..=MAX_STAGES)
            .map(|i| json!({"key": format!("stage-{i}"), "minimum_amount": format!("{i}.00")}))
            .collect();
        for (field, value) in [
            ("schema_version", json!(2)),
            ("schema_version", json!("1")),
            ("revision", json!(0)),
            ("revision", json!(i64::from(i32::MAX) + 1)),
            ("currency", json!("USD")),
            ("tenant_id", json!(Uuid::nil())),
            (
                "organization_id",
                json!("3E793FB5-75A7-45F6-8F66-45A58FCEC674"),
            ),
            ("policy_id", json!("b3c1837d138f4e229b6ac450aced37bc")),
            ("require_distinct_approvers", json!("false")),
            ("stages", json!([])),
            ("stages", Value::Array(too_many)),
            ("unknown", json!(true)),
        ] {
            let mut changed = valid.clone();
            changed[field] = value;
            assert_eq!(
                parse(&changed).unwrap_err(),
                Failure::InvalidPolicy,
                "{field}"
            );
        }
        for field in valid.as_object().unwrap().keys() {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert_eq!(
                parse(&missing).unwrap_err(),
                Failure::InvalidPolicy,
                "{field}"
            );
        }
        let mut grant = valid.clone();
        grant["stages"][0]["permission"] = json!("procurement.issue");
        assert_eq!(parse(&grant).unwrap_err(), Failure::InvalidPolicy);
    }

    #[test]
    fn enforces_stage_key_and_threshold_rules() {
        for invalid in [
            json!(""),
            json!("Budget"),
            json!("1budget"),
            json!("büdget"),
            json!("budget\n"),
            json!("procurement.issue"),
            json!("b".repeat(33)),
            json!("department"),
        ] {
            let mut value = example();
            value["stages"][1]["key"] = invalid.clone();
            assert_eq!(
                parse(&value).unwrap_err(),
                Failure::InvalidPolicy,
                "{invalid}"
            );
        }
        for invalid in [
            json!(1500),
            json!("1E3"),
            json!("1500.0"),
            json!("1500.001"),
            json!("01500.00"),
            json!("-0.00"),
            json!("+1.00"),
            json!("1.00\n"),
            json!("０.００"),
            json!("1000000000000000.00"),
            json!("0.00"),
        ] {
            let mut value = example();
            value["stages"][1]["minimum_amount"] = invalid.clone();
            assert_eq!(
                parse(&value).unwrap_err(),
                Failure::InvalidPolicy,
                "{invalid}"
            );
        }
        let mut value = example();
        value["stages"][0]["minimum_amount"] = json!("0.01");
        assert_eq!(parse(&value).unwrap_err(), Failure::InvalidPolicy);
        value["stages"][0]["minimum_amount"] = json!("0.00");
        value["stages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"key": "third", "minimum_amount": "1499.99"}));
        assert_eq!(parse(&value).unwrap_err(), Failure::InvalidPolicy);
    }

    #[test]
    fn accepts_eight_stages_and_exact_upper_limits() {
        let mut value = example();
        for index in 2..MAX_STAGES - 1 {
            value["stages"].as_array_mut().unwrap().push(json!({
                "key": format!("stage-{index}"),
                "minimum_amount": format!("{}.00", 1500 + index),
            }));
        }
        let last_key = "a".repeat(MAX_KEY_BYTES);
        value["stages"].as_array_mut().unwrap().push(json!({
            "key": last_key,
            "minimum_amount": "999999999999999.99",
        }));
        value["revision"] = json!(i32::MAX);
        let policy = parse(&value).unwrap();
        let max = 99_999_999_999_999_999;
        let below = policy.requirements(scope(), "EUR", max - 1).unwrap();
        assert_eq!(below.stages().len(), 7);
        let at = policy.requirements(scope(), "EUR", max).unwrap();
        assert_eq!(at.policy_revision(), i32::MAX);
        assert_eq!(at.stages().len(), 8);
        assert_eq!(at.stages().last().unwrap().key(), last_key);
        let prefix = [decision("department", FIRST), decision("budget", SECOND)];
        assert_eq!(
            at.validate_next(EDITOR, &prefix, decision("stage-2", FIRST))
                .unwrap_err(),
            Failure::RepeatedApprover
        );
    }

    #[test]
    fn refuses_wrong_verified_scope_or_persisted_currency() {
        let policy = Policy::parse(Some(EXAMPLE)).unwrap();
        for foreign in [
            Scope {
                tenant_id: FIRST,
                ..scope()
            },
            Scope {
                organization_id: SECOND,
                ..scope()
            },
        ] {
            assert_eq!(
                policy.requirements(foreign, "EUR", 188_240).unwrap_err(),
                Failure::ScopeMismatch
            );
        }
        for currency in ["USD", "eur", ""] {
            assert_eq!(
                policy.requirements(scope(), currency, 188_240).unwrap_err(),
                Failure::CurrencyMismatch
            );
        }
    }

    #[test]
    fn only_the_next_stage_advances_and_a_valid_full_prefix_completes() {
        let policy = Policy::parse(Some(EXAMPLE)).unwrap();
        let required = policy.requirements(scope(), "EUR", 188_240).unwrap();
        let first = || decision("department", FIRST);
        let second = || decision("budget", SECOND);
        assert_eq!(
            required.next_stage(EDITOR, &[]).unwrap().unwrap().key(),
            "department"
        );
        assert_eq!(
            required.validate_next(EDITOR, &[], second()).unwrap_err(),
            Failure::NotNextStage
        );
        assert_eq!(
            required
                .validate_next(EDITOR, &[], first())
                .unwrap()
                .permission(),
            "procurement.approval.department"
        );
        assert_eq!(
            required
                .next_stage(EDITOR, &[first()])
                .unwrap()
                .unwrap()
                .key(),
            "budget"
        );
        assert_eq!(
            required
                .validate_next(EDITOR, &[first()], first())
                .unwrap_err(),
            Failure::NotNextStage
        );
        assert_eq!(
            required
                .validate_next(EDITOR, &[first()], second())
                .unwrap()
                .permission(),
            "procurement.approval.budget"
        );
        let completed = [first(), second()];
        assert!(required.next_stage(EDITOR, &completed).unwrap().is_none());
        assert_eq!(
            required
                .validate_next(EDITOR, &completed, second())
                .unwrap_err(),
            Failure::NotNextStage
        );
        for invalid in [
            vec![second()],
            vec![second(), first()],
            vec![first(), first()],
            vec![first(), second(), second()],
        ] {
            assert_eq!(
                required.next_stage(EDITOR, &invalid).unwrap_err(),
                Failure::InvalidPrefix
            );
        }
        let below = policy.requirements(scope(), "EUR", 149_999).unwrap();
        assert!(below.next_stage(EDITOR, &[first()]).unwrap().is_none());
        assert_eq!(
            below.next_stage(EDITOR, &completed).unwrap_err(),
            Failure::InvalidPrefix
        );
    }

    #[test]
    fn always_rejects_terms_editor_and_obeys_explicit_distinct_approvers_rule() {
        for distinct in [true, false] {
            let mut value = example();
            value["require_distinct_approvers"] = json!(distinct);
            let policy = parse(&value).unwrap();
            let required = policy.requirements(scope(), "EUR", 188_240).unwrap();
            let first = || decision("department", FIRST);
            let repeated = || decision("budget", FIRST);
            assert_eq!(
                required
                    .validate_next(EDITOR, &[], decision("department", EDITOR))
                    .unwrap_err(),
                Failure::SelfApproval
            );
            assert_eq!(
                required
                    .validate_next(EDITOR, &[first()], decision("budget", EDITOR))
                    .unwrap_err(),
                Failure::SelfApproval
            );
            assert_eq!(
                required
                    .next_stage(EDITOR, &[first(), decision("budget", EDITOR)])
                    .unwrap_err(),
                Failure::SelfApproval
            );
            assert_eq!(
                required.next_stage(Uuid::nil(), &[]).unwrap_err(),
                Failure::InvalidActor
            );
            assert_eq!(
                required
                    .validate_next(EDITOR, &[], decision("department", Uuid::nil()))
                    .unwrap_err(),
                Failure::InvalidActor
            );
            let candidate = required.validate_next(EDITOR, &[first()], repeated());
            let prefix = required.next_stage(EDITOR, &[first(), repeated()]);
            if distinct {
                assert_eq!(candidate.unwrap_err(), Failure::RepeatedApprover);
                assert_eq!(prefix.unwrap_err(), Failure::RepeatedApprover);
            } else {
                assert!(candidate.is_ok());
                assert!(prefix.unwrap().is_none());
            }
        }
    }
}
