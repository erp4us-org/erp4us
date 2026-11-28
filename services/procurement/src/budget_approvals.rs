use crate::{
    Scope,
    budget_policy::{self, CompletedStage, Policy, PolicySnapshot, Requirements},
    material_plan::canonical_id,
    terms::Saved,
};
use mongodb::bson::DateTime;
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::File,
    io::{self, Read},
};
use uuid::Uuid;

const POLICY_FILE: &str = "ERP4US_PROCUREMENT_BUDGET_POLICY";
const MAX_POLICY_BYTES: usize = 16 * 1024;

pub(super) fn load_policy() -> io::Result<Option<Policy>> {
    let Some(path) = env::var_os(POLICY_FILE) else {
        return Ok(None);
    };
    let file =
        File::open(path).map_err(|_| io::Error::other("Cannot read Procurement budget policy"))?;
    read_policy(file).map(Some)
}

fn read_policy(input: impl Read) -> io::Result<Policy> {
    let mut bytes = Vec::new();
    input
        .take(MAX_POLICY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| io::Error::other("Cannot read Procurement budget policy"))?;
    if bytes.len() > MAX_POLICY_BYTES {
        return Err(io::Error::other(
            "Procurement budget policy exceeds size limit",
        ));
    }
    Policy::parse(Some(&bytes)).map_err(|_| io::Error::other("Invalid Procurement budget policy"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Failure {
    Invalid,
    Forbidden,
    NotFound,
    Conflict,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Intent {
    #[serde(with = "canonical_id")]
    pub command_id: Uuid,
    pub terms_revision: i32,
    #[serde(with = "canonical_id")]
    pub policy_id: Uuid,
    pub policy_revision: i32,
    pub stage_key: String,
}

pub(super) struct Command {
    pub intent: Intent,
    permission: String,
}

impl Intent {
    pub(super) fn authorizing_operation(self) -> Result<Command, Failure> {
        let permission =
            budget_policy::stage_permission(&self.stage_key).ok_or(Failure::Invalid)?;
        if self.terms_revision < 1 || self.policy_revision < 1 {
            return Err(Failure::Invalid);
        }
        Ok(Command {
            intent: self,
            permission,
        })
    }
}

impl Command {
    pub(super) fn permission(&self) -> &str {
        &self.permission
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Approval {
    #[serde(with = "canonical_id")]
    proposal_id: Uuid,
    #[serde(with = "canonical_id")]
    plan_id: Uuid,
    terms_revision: i32,
    #[serde(with = "canonical_id")]
    policy_id: Uuid,
    policy_revision: i32,
    currency: String,
    total: String,
    state: State,
    stages: Vec<StageStatus>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct StageStatus {
    key: String,
    state: State,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum State {
    Approved,
    Pending,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Record {
    pub intent: Intent,
    #[serde(with = "canonical_id")]
    actor_id: Uuid,
    stage_index: i32,
    decided_at: DateTime,
    schema_version: i32,
    policy_snapshot: PolicySnapshot,
    pub original_result: Approval,
}

impl Record {
    pub(super) fn replay(
        self,
        command: &Command,
        actor_id: Uuid,
        current: &Review<'_>,
    ) -> Result<Approval, Failure> {
        if self.schema_version != 1
            || self.original_result.proposal_id != current.terms.terms.proposal_id
            || self.original_result.plan_id != current.terms.terms.plan_id
        {
            return Err(Failure::Unavailable);
        }
        if self.intent != command.intent || self.actor_id != actor_id {
            return Err(Failure::Conflict);
        }
        // A different published version can replay history. Mutating the
        // same version is incompatible configuration, not a new policy.
        if self.intent.policy_id == current.requirements.policy_id()
            && self.intent.policy_revision == current.requirements.policy_revision()
            && self.policy_snapshot != current.snapshot
        {
            return Err(Failure::Unavailable);
        }
        Ok(self.original_result)
    }
}

pub(super) struct Review<'a> {
    terms: &'a Saved,
    requirements: Requirements<'a>,
    snapshot: PolicySnapshot,
    currency: &'static str,
    total: String,
}

impl<'a> Review<'a> {
    pub(super) fn new(
        scope: Scope,
        terms: &'a Saved,
        policy: Option<&'a Policy>,
    ) -> Result<Self, Failure> {
        let policy = policy.ok_or(Failure::Unavailable)?;
        let (currency, cents, total) = terms.terms.budget_amount();
        let requirements = policy
            .requirements(scope, currency, cents)
            .map_err(|_| Failure::Unavailable)?;
        if terms.terms.revision < 1 {
            return Err(Failure::Unavailable);
        }
        Ok(Self {
            terms,
            requirements,
            snapshot: policy.snapshot(),
            currency,
            total,
        })
    }

    pub(super) fn version(&self) -> (i32, Uuid, i32) {
        (
            self.terms.terms.revision,
            self.requirements.policy_id(),
            self.requirements.policy_revision(),
        )
    }

    pub(super) fn check_version(&self, intent: &Intent) -> Result<(), Failure> {
        (self.version()
            == (
                intent.terms_revision,
                intent.policy_id,
                intent.policy_revision,
            ))
            .then_some(())
            .ok_or(Failure::Conflict)
    }

    fn prefix<'d>(&self, decisions: &'d [Record]) -> Result<Vec<CompletedStage<'d>>, Failure> {
        if decisions.len() > self.requirements.stages().len() {
            return Err(Failure::Unavailable);
        }
        let mut prefix = Vec::with_capacity(decisions.len());
        for (index, decision) in decisions.iter().enumerate() {
            if decision.schema_version != 1
                || decision.stage_index != index as i32 + 1
                || decision.policy_snapshot != self.snapshot
                || self.check_version(&decision.intent).is_err()
                || decision.original_result != self.result_at(index + 1)
            {
                return Err(Failure::Unavailable);
            }
            prefix.push(CompletedStage {
                key: &decision.intent.stage_key,
                actor_id: decision.actor_id,
            });
        }
        self.requirements
            .next_stage(self.terms.actor_id, &prefix)
            .map_err(|_| Failure::Unavailable)?;
        Ok(prefix)
    }

    pub(super) fn result(&self, decisions: &[Record]) -> Result<Approval, Failure> {
        let prefix = self.prefix(decisions)?;
        Ok(self.result_at(prefix.len()))
    }

    /// Validate the entire approval prefix before allowing its bound terms to
    /// be used for issuance. `prefix` verifies each original result against
    /// the current review, including the whole draft total and exact versions.
    pub(super) fn approved_terms(&self, decisions: &[Record]) -> Result<(&'a Saved, u64), Failure> {
        let prefix = self.prefix(decisions)?;
        if prefix.len() != self.requirements.stages().len() {
            return Err(Failure::Conflict);
        }
        Ok((self.terms, self.terms.terms.total().as_u64()))
    }

    pub(super) fn decide(
        &self,
        decisions: &[Record],
        command: &Command,
        actor_id: Uuid,
    ) -> Result<Record, Failure> {
        self.check_version(&command.intent)?;
        let prefix = self.prefix(decisions)?;
        let stage = self
            .requirements
            .validate_next(
                self.terms.actor_id,
                &prefix,
                CompletedStage {
                    key: &command.intent.stage_key,
                    actor_id,
                },
            )
            .map_err(|error| match error {
                budget_policy::Failure::NotNextStage => Failure::Conflict,
                budget_policy::Failure::InvalidActor
                | budget_policy::Failure::SelfApproval
                | budget_policy::Failure::RepeatedApprover => Failure::Forbidden,
                _ => Failure::Unavailable,
            })?;
        if stage.permission() != command.permission() {
            return Err(Failure::Forbidden);
        }
        let completed = prefix.len() + 1;
        Ok(Record {
            intent: command.intent.clone(),
            actor_id,
            stage_index: completed as i32,
            decided_at: DateTime::now(),
            schema_version: 1,
            policy_snapshot: self.snapshot.clone(),
            original_result: self.result_at(completed),
        })
    }

    fn result_at(&self, completed: usize) -> Approval {
        let stages = self.requirements.stages();
        Approval {
            proposal_id: self.terms.terms.proposal_id,
            plan_id: self.terms.terms.plan_id,
            terms_revision: self.terms.terms.revision,
            policy_id: self.requirements.policy_id(),
            policy_revision: self.requirements.policy_revision(),
            currency: self.currency.to_owned(),
            total: self.total.clone(),
            state: if completed == stages.len() {
                State::Approved
            } else {
                State::Pending
            },
            stages: stages
                .iter()
                .enumerate()
                .map(|(index, stage)| StageStatus {
                    key: stage.key().to_owned(),
                    state: if index < completed {
                        State::Approved
                    } else {
                        State::Pending
                    },
                })
                .collect(),
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::budget_policy::JsonObject;
    use serde_json::{Value, json};
    use std::collections::HashMap;

    const POLICY: &[u8] =
        include_bytes!("../../../contracts/examples/procurement/budget-policy-v1.json");
    const EDITOR: Uuid = Uuid::from_u128(1);
    pub(crate) const APPROVER: Uuid = Uuid::from_u128(2);
    const OTHER_APPROVER: Uuid = Uuid::from_u128(3);

    pub(crate) fn terms_fixture(revision: i32) -> (Scope, Saved) {
        let (scope, draft, mut input, supplier) = crate::terms::tests::fixture();
        input["expected_revision"] = json!(revision - 1);
        input["lines"][1]["unit_price"] = json!("300.00");
        let terms = crate::terms::tests::command(&input)
            .unwrap()
            .bind(&draft)
            .unwrap()
            .snapshot(revision, &HashMap::from([(supplier.id, supplier)]), EDITOR)
            .unwrap();
        (scope, terms)
    }

    pub(crate) fn policy() -> Policy {
        read_policy(POLICY).unwrap()
    }

    pub(crate) fn stage(review: &Review<'_>, key: &str) -> Command {
        let (terms_revision, policy_id, policy_revision) = review.version();
        Intent {
            command_id: Uuid::new_v4(),
            terms_revision,
            policy_id,
            policy_revision,
            stage_key: key.to_owned(),
        }
        .authorizing_operation()
        .unwrap()
    }

    #[test]
    fn command_accepts_only_exact_versions_and_ascii_stage_operation() {
        let (scope, terms) = terms_fixture(1);
        let policy = policy();
        let review = Review::new(scope, &terms, Some(&policy)).unwrap();
        let command = stage(&review, "department");
        assert_eq!(command.permission(), "procurement.approval.department");
        let input = serde_json::to_value(command.intent).unwrap();
        let parse = |value: Value| {
            let JsonObject(intent) = serde_json::from_value::<JsonObject<Intent>>(value)
                .map_err(|_| Failure::Invalid)?;
            intent.authorizing_operation()
        };
        assert!(parse(input.clone()).is_ok());
        for key in [
            "actor_id",
            "tenant_id",
            "total",
            "permission",
            "minimum_amount",
        ] {
            let mut extra = input.clone();
            extra[key] = json!("not allowed");
            assert!(matches!(parse(extra), Err(Failure::Invalid)));
        }
        for key in [
            "",
            "Department",
            "department\n",
            "département",
            "department.budget",
            "department-*",
            "-budget",
            "department-department-department-1",
        ] {
            let mut invalid = input.clone();
            invalid["stage_key"] = json!(key);
            assert!(matches!(parse(invalid), Err(Failure::Invalid)));
        }
        for (key, bad_value) in [
            ("terms_revision", json!(0)),
            ("policy_revision", json!(-1)),
            ("policy_id", json!(Uuid::nil())),
            ("command_id", json!("000000000000000000000000000000ab")),
        ] {
            let mut invalid = input.clone();
            invalid[key] = bad_value;
            assert!(matches!(parse(invalid), Err(Failure::Invalid)));
        }
        assert!(matches!(parse(json!([])), Err(Failure::Invalid)));
        let duplicate =
            serde_json::to_string(&input)
                .unwrap()
                .replacen('{', r#"{"stage_key":"repeated","#, 1);
        assert!(serde_json::from_str::<JsonObject<Intent>>(&duplicate).is_err());
    }

    #[test]
    fn stages_bind_exact_saved_total_terms_editor_order_and_actor_separation() {
        let (scope, terms) = terms_fixture(1);
        let policy = policy();
        let review = Review::new(scope, &terms, Some(&policy)).unwrap();
        let first = stage(&review, "department");
        let second = stage(&review, "budget");

        assert_eq!(
            review.decide(&[], &second, APPROVER),
            Err(Failure::Conflict)
        );
        assert_eq!(review.decide(&[], &first, EDITOR), Err(Failure::Forbidden));
        let first_record = review.decide(&[], &first, APPROVER).unwrap();
        assert_eq!(
            serde_json::to_value(&first_record.original_result).unwrap(),
            json!({
                "proposal_id": terms.terms.proposal_id, "plan_id": terms.terms.plan_id,
                "terms_revision": 1, "policy_id": first.intent.policy_id, "policy_revision": 1,
                "currency": "EUR", "total": "1902.36", "state": "pending",
                "stages": [
                    {"key": "department", "state": "approved"},
                    {"key": "budget", "state": "pending"},
                ]
            })
        );
        let decisions = std::slice::from_ref(&first_record);
        assert_eq!(
            review.decide(decisions, &first, OTHER_APPROVER),
            Err(Failure::Conflict)
        );
        assert_eq!(
            review.decide(decisions, &second, APPROVER),
            Err(Failure::Forbidden)
        );
        let second_record = review.decide(decisions, &second, OTHER_APPROVER).unwrap();
        let completed = [first_record.clone(), second_record.clone()];
        assert_eq!(review.result(&completed).unwrap().state, State::Approved);
        assert_eq!(
            review.decide(&completed, &second, OTHER_APPROVER),
            Err(Failure::Conflict)
        );
        assert_eq!(
            review.result(&[second_record, first_record]),
            Err(Failure::Unavailable)
        );
    }

    #[test]
    fn replay_keeps_original_result_while_stale_new_intents_cannot_write() {
        let (scope, mut terms) = terms_fixture(1);
        let policy = policy();
        let review = Review::new(scope, &terms, Some(&policy)).unwrap();
        let first = stage(&review, "department");
        let receipt = review.decide(&[], &first, APPROVER).unwrap();
        let another_command = stage(&review, "budget");
        let mut disputed = first.intent.clone();
        disputed.stage_key = "budget".to_owned();
        assert_eq!(
            receipt.clone().replay(
                &disputed.authorizing_operation().unwrap(),
                APPROVER,
                &review,
            ),
            Err(Failure::Conflict)
        );
        assert_eq!(
            receipt.clone().replay(&first, OTHER_APPROVER, &review),
            Err(Failure::Conflict)
        );

        let mut config: Value = serde_json::from_slice(POLICY).unwrap();
        config["stages"][1]["minimum_amount"] = json!("1600.00");
        let incompatible = read_policy(serde_json::to_vec(&config).unwrap().as_slice()).unwrap();
        let incompatible = Review::new(scope, &terms, Some(&incompatible)).unwrap();
        assert_eq!(
            incompatible.result(std::slice::from_ref(&receipt)),
            Err(Failure::Unavailable)
        );
        assert_eq!(
            receipt.clone().replay(&first, APPROVER, &incompatible),
            Err(Failure::Unavailable)
        );

        terms.terms.revision = 2;
        config["revision"] = json!(2);
        let revised = read_policy(serde_json::to_vec(&config).unwrap().as_slice()).unwrap();
        let current = Review::new(scope, &terms, Some(&revised)).unwrap();
        assert_eq!(
            receipt.clone().replay(&first, APPROVER, &current),
            Ok(receipt.original_result)
        );
        assert_eq!(
            current.decide(&[], &another_command, OTHER_APPROVER),
            Err(Failure::Conflict)
        );
        assert!(
            current
                .result(&[])
                .unwrap()
                .stages
                .iter()
                .all(|s| s.state == State::Pending)
        );
    }

    #[test]
    fn policy_load_and_scope_fail_closed() {
        let (scope, terms) = terms_fixture(1);
        let policy = policy();
        assert!(matches!(
            Review::new(scope, &terms, None),
            Err(Failure::Unavailable)
        ));
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
            assert!(matches!(
                Review::new(foreign, &terms, Some(&policy)),
                Err(Failure::Unavailable)
            ));
        }
        let mut oversized = POLICY.to_vec();
        oversized.resize(MAX_POLICY_BYTES + 1, b' ');
        assert!(read_policy(oversized.as_slice()).is_err());
        assert!(read_policy(b"{}".as_slice()).is_err());
    }
}
