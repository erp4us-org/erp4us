//! Pure projection of committed Inventory receipts, not a stock or order balance authority.
//!
//! A future adapter must first authorize the producing principal and scope, validate the
//! wire contract, and load the exact immutable issued line from Procurement. It must
//! serialize writers, enforce durable uniqueness, and commit inbox/staging and projection
//! changes together before ACK. None of these responsibilities is a broker guarantee.
//!
//! Admission needs three indexed lookups, never a history scan. Apply at most a bounded
//! number of indexed next rows per transaction; durably continue unfinished work and
//! reconcile missing rows. This kernel retains no facts or retention policy.

#![cfg(test)]

use crate::{
    Scope,
    material_plan::{pieces as valid_pieces, revision, sku},
    receipt_wire::{MAX_BODY_BYTES, ReceiptPosted},
};
use erp4us_messaging::Envelope;
use uuid::Uuid;

const MAX_SEQUENCE: u32 = 1_000_000;

mod store;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LineKey<'a> {
    // These are the verified scope on each side, not authority inferred from IDs.
    tenant_id: Uuid,
    organization_id: Uuid,
    plan_id: Uuid,
    purchase_order_id: Uuid,
    purchase_line_id: Uuid,
    sku: &'a str,
    revision: Option<&'a str>,
    unit: &'a str,
}

impl LineKey<'_> {
    fn validate(self) -> Result<(), Invalid> {
        if [
            self.tenant_id,
            self.organization_id,
            self.plan_id,
            self.purchase_order_id,
            self.purchase_line_id,
        ]
        .iter()
        .any(Uuid::is_nil)
            || !sku(self.sku)
            || !self.revision.is_none_or(revision)
            || self.unit != "EA"
        {
            return Err(Invalid::Line);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Invalid {
    Scope,
    Line,
    Pieces,
    Identity,
}

fn positive_pieces(value: &str) -> Result<u32, Invalid> {
    if !valid_pieces(value) {
        return Err(Invalid::Pieces);
    }
    value.parse().map_err(|_| Invalid::Pieces)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Identity {
    message_id: Uuid,
    receipt_id: Uuid,
    sequence: u32,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct VerifiedReceipt<'a> {
    line: LineKey<'a>,
    id: Identity,
    pieces: u32,
    owner_content: &'a [u8],
}

impl<'a> VerifiedReceipt<'a> {
    /// Supply only a positively verified committed receipt and its independently verified
    /// `scope`. `owner_content` is a lossless comparable representation of
    /// immutable owner attestation not already held by the typed receipt.
    /// Transport replay headers and diagnostic metadata do not belong in it.
    /// These local shape checks are not a wire validator or a proof of authority.
    fn from_verified(
        scope: Scope,
        line: LineKey<'a>,
        id: Identity,
        pieces: &str,
        owner_content: &'a [u8],
    ) -> Result<Self, Invalid> {
        line.validate()?;
        if (scope.tenant_id, scope.organization_id) != (line.tenant_id, line.organization_id) {
            return Err(Invalid::Scope);
        }
        if id.message_id.is_nil()
            || id.receipt_id.is_nil()
            || id.message_id == id.receipt_id
            || !(1..=MAX_SEQUENCE).contains(&id.sequence)
            || !(1..=MAX_BODY_BYTES).contains(&owner_content.len())
        {
            return Err(Invalid::Identity);
        }
        Ok(Self {
            line,
            id,
            pieces: positive_pieces(pieces)?,
            owner_content,
        })
    }

    /// Convert the decoded owner wire shape. The consumer still must authorize
    /// the publication principal, and compare the result to the issued line.
    fn from_decoded(
        scope: Scope,
        event: &'a Envelope<ReceiptPosted>,
        owner_content: &'a [u8],
    ) -> Result<Self, Invalid> {
        let row = &event.payload;
        let line = LineKey {
            tenant_id: event.tenant_id.ok_or(Invalid::Scope)?,
            organization_id: event.organization_id.ok_or(Invalid::Scope)?,
            plan_id: row.plan_id,
            purchase_order_id: row.purchase_order_id,
            purchase_line_id: row.purchase_line_id,
            sku: &row.sku,
            revision: row.revision.as_deref(),
            unit: &row.unit,
        };
        let id = Identity {
            message_id: event.message_id,
            receipt_id: row.receipt_id,
            sequence: row.receipt_sequence,
        };
        Self::from_verified(scope, line, id, &row.pieces, owner_content)
    }
}

/// Authoritative inbox lookups in the same transaction (including already applied facts):
/// by scoped message ID, scoped receipt ID, and scoped plan/order/line/sequence.
/// Lookup failures must not be presented as absence.
#[derive(Default)]
struct Existing<'a> {
    message: Option<VerifiedReceipt<'a>>,
    receipt: Option<VerifiedReceipt<'a>>,
    sequence: Option<VerifiedReceipt<'a>>,
}

#[derive(Debug, Eq, PartialEq)]
enum Conflict {
    Message,
    Receipt,
    Sequence,
    OverOrdered,
}

#[derive(Debug, Eq, PartialEq)]
enum Admission {
    Staged,
    Replay,
    NotForLine,
    Blocked,
    Conflict(Conflict),
}

#[derive(Debug, Eq, PartialEq)]
enum ApplyFailure {
    NotForLine,
    Blocked,
    NotNext,
    InconsistentStage,
}

#[derive(Debug, Eq, PartialEq)]
enum Observation {
    Unknown,
    Observed {
        received_pieces: u32,
        open_pieces: u32,
        through_receipt_sequence: u32,
    },
}

struct Projection<'a> {
    line: LineKey<'a>,
    ordered: u32,
    highest_known: u32,
    // Include all admitted facts, staged or applied, to bound new admissions.
    admitted_count: u32,
    admitted_pieces: u32,
    through: u32,
    received: u32,
    blocked: bool,
}

impl<'a> Projection<'a> {
    /// The initial state for an immutable issued line, not a guess at its receipt balance.
    fn new(line: LineKey<'a>, ordered_pieces: &str) -> Result<Self, Invalid> {
        line.validate()?;
        Ok(Self {
            line,
            ordered: positive_pieces(ordered_pieces)?,
            highest_known: 0,
            admitted_count: 0,
            admitted_pieces: 0,
            through: 0,
            received: 0,
            blocked: false,
        })
    }

    /// Persist a staged fact with the summary, or persist blocking and a minimal scoped
    /// quarantine reference on conflict. An invalid/unrelated input must not block a
    /// legitimate line. When IDs collide across lines, only separately verified
    /// affected lines may also be blocked; never follow the rejected claims blindly.
    fn stage(&mut self, fact: VerifiedReceipt<'_>, existing: Existing<'_>) -> Admission {
        if fact.line != self.line {
            return Admission::NotForLine;
        }
        let sequence = fact.id.sequence;
        let highest_known = self.highest_known.max(sequence);
        let admitted_count = self.admitted_count + 1;
        let admitted_pieces = self.admitted_pieces + fact.pieces;
        let conflict = if let Some(message) = existing.message {
            if message == fact && existing.receipt == Some(fact) && existing.sequence == Some(fact)
            {
                return Admission::Replay;
            }
            Conflict::Message
        } else if existing.receipt.is_some() {
            Conflict::Receipt
        } else if existing.sequence.is_some() || sequence <= self.through {
            Conflict::Sequence
        } else if self.blocked {
            return Admission::Blocked;
        } else if let Some(missing) = highest_known.checked_sub(admitted_count) {
            // Every gap must contain at least one piece. Don't stage a fact
            // that is already impossible given the other known facts.
            if admitted_pieces + missing <= self.ordered {
                self.highest_known = highest_known;
                self.admitted_count = admitted_count;
                self.admitted_pieces = admitted_pieces;
                return Admission::Staged;
            }
            Conflict::OverOrdered
        } else {
            Conflict::Sequence
        };
        self.block();
        Admission::Conflict(conflict)
    }

    /// Call only on the separately verified affected line for unresolved integrity or
    /// reconciliation trouble. There is no blind unblock on delivery, replay or ACK.
    fn block(&mut self) {
        self.blocked = true;
    }

    fn next_to_apply(&self) -> Option<u32> {
        (!self.blocked && self.through < self.highest_known).then_some(self.through + 1)
    }

    /// The caller must supply the authoritative staged inbox row for `next_to_apply`,
    /// not a delivery that has yet to be staged. No row at that index means a known gap.
    /// Persist the projection time only when the prefix advances; it is not a freshness check.
    fn apply_next(&mut self, staged: VerifiedReceipt<'_>) -> Result<(), ApplyFailure> {
        if staged.line != self.line {
            return Err(ApplyFailure::NotForLine);
        }
        if self.blocked {
            return Err(ApplyFailure::Blocked);
        }
        if self.next_to_apply() != Some(staged.id.sequence) {
            return Err(ApplyFailure::NotNext);
        }
        let received = self.received + staged.pieces;
        let sequence = staged.id.sequence;
        if received > self.admitted_pieces
            || (sequence == self.highest_known
                && (sequence != self.admitted_count || received != self.admitted_pieces))
        {
            self.block();
            return Err(ApplyFailure::InconsistentStage);
        }
        self.received = received;
        self.through = sequence;
        Ok(())
    }

    fn observation(&self) -> Observation {
        if self.blocked || self.through == 0 || self.through != self.highest_known {
            return Observation::Unknown;
        }
        Observation::Observed {
            received_pieces: self.received,
            open_pieces: self.ordered - self.received,
            through_receipt_sequence: self.through,
        }
    }
}

mod tests {
    use super::*;

    fn line() -> LineKey<'static> {
        LineKey {
            tenant_id: Uuid::from_u128(1),
            organization_id: Uuid::from_u128(2),
            plan_id: Uuid::from_u128(3),
            purchase_order_id: Uuid::from_u128(4),
            purchase_line_id: Uuid::from_u128(5),
            sku: "NM-END-PLATE",
            revision: Some("A"),
            unit: "EA",
        }
    }

    fn scope(line: LineKey<'_>) -> Scope {
        Scope {
            tenant_id: line.tenant_id,
            organization_id: line.organization_id,
        }
    }

    fn receipt(sequence: u32, pieces: &str) -> VerifiedReceipt<'static> {
        VerifiedReceipt::from_verified(
            scope(line()),
            line(),
            Identity {
                message_id: Uuid::from_u128(10 + u128::from(sequence)),
                receipt_id: Uuid::from_u128(20 + u128::from(sequence)),
                sequence,
            },
            pieces,
            b"verified immutable envelope metadata and payload fixture",
        )
        .ok()
        .unwrap()
    }

    fn existing(fact: VerifiedReceipt<'_>) -> Existing<'_> {
        Existing {
            message: Some(fact),
            receipt: Some(fact),
            sequence: Some(fact),
        }
    }

    fn observed(received: u32, open: u32, through: u32) -> Observation {
        Observation::Observed {
            received_pieces: received,
            open_pieces: open,
            through_receipt_sequence: through,
        }
    }

    fn stage_and_apply(projection: &mut Projection<'_>, fact: VerifiedReceipt<'_>) {
        assert_eq!(
            projection.stage(fact, Existing::default()),
            Admission::Staged
        );
        projection.apply_next(fact).unwrap();
    }

    #[test]
    fn independently_decoded_receipt_and_operator_replay_leave_one_observed_effect() {
        let (scope, subject, mut headers, fixture) = crate::receipt_wire::tests::fixture();
        let bytes = serde_json::to_vec(&fixture).unwrap();
        let event = crate::receipt_wire::decode(scope, &subject, &headers, &bytes).unwrap();
        let normalized = serde_json::to_vec(&event).unwrap();
        let fact = VerifiedReceipt::from_decoded(scope, &event, &normalized).unwrap();
        let line = LineKey {
            sku: "NM-ENDPLATE",
            revision: None,
            ..fact.line
        };
        let mut projection = Projection::new(line, "12").unwrap();
        stage_and_apply(&mut projection, fact);

        headers.insert(
            "Nats-Msg-Id",
            format!("replay-38-{}", event.message_id).as_str(),
        );
        let replay = crate::receipt_wire::decode(scope, &subject, &headers, &bytes).unwrap();
        let normalized_replay = serde_json::to_vec(&replay).unwrap();
        let replay = VerifiedReceipt::from_decoded(scope, &replay, &normalized_replay).unwrap();
        assert_eq!(projection.stage(replay, existing(fact)), Admission::Replay);
        assert_eq!(projection.observation(), observed(6, 6, 1));
    }

    #[test]
    fn no_observation_then_partial_then_full_even_with_replay() {
        let mut projection = Projection::new(line(), "12").unwrap();
        assert_eq!(projection.observation(), Observation::Unknown);

        let first = receipt(1, "6");
        stage_and_apply(&mut projection, first);
        assert_eq!(projection.observation(), observed(6, 6, 1));
        assert_eq!(projection.stage(first, existing(first)), Admission::Replay);
        assert_eq!(projection.observation(), observed(6, 6, 1));

        stage_and_apply(&mut projection, receipt(2, "6"));
        assert_eq!(projection.observation(), observed(12, 0, 2));
        assert_eq!(projection.stage(first, existing(first)), Admission::Replay);
        assert_eq!(projection.observation(), observed(12, 0, 2));
    }

    #[test]
    fn second_arrival_does_not_skip_the_first_sequence() {
        let mut projection = Projection::new(line(), "12").unwrap();
        let second = receipt(2, "6");
        assert_eq!(
            projection.stage(second, Existing::default()),
            Admission::Staged
        );
        assert_eq!(
            projection.stage(second, existing(second)),
            Admission::Replay
        );
        assert_eq!(projection.next_to_apply(), Some(1));
        assert_eq!(projection.apply_next(second), Err(ApplyFailure::NotNext));
        assert_eq!(projection.observation(), Observation::Unknown);

        stage_and_apply(&mut projection, receipt(1, "6"));
        assert_eq!(projection.observation(), Observation::Unknown);
        assert_eq!(projection.next_to_apply(), Some(2));
        projection.apply_next(second).unwrap();
        assert_eq!(projection.observation(), observed(12, 0, 2));
        assert_eq!(projection.next_to_apply(), None);
    }

    #[test]
    fn inconsistent_staged_row_cannot_complete_the_prefix() {
        let mut projection = Projection::new(line(), "12").unwrap();
        assert_eq!(
            projection.stage(receipt(1, "6"), Existing::default()),
            Admission::Staged
        );
        assert_eq!(
            projection.apply_next(receipt(1, "5")),
            Err(ApplyFailure::InconsistentStage)
        );
        assert_eq!((projection.received, projection.through), (0, 0));
        assert_eq!(projection.observation(), Observation::Unknown);
        assert_eq!(projection.next_to_apply(), None);
    }

    #[test]
    fn conflicts_block_the_verified_line_without_advancing() {
        let first = receipt(1, "6");
        let second = receipt(2, "6");
        let mut changed_envelope = first;
        changed_envelope.owner_content = b"changed metadata";
        let mut reused_receipt = second;
        reused_receipt.id.receipt_id = first.id.receipt_id;
        let mut reused_sequence = second;
        reused_sequence.id.sequence = first.id.sequence;

        for (fact, found, reason) in [
            (receipt(1, "5"), existing(first), Conflict::Message),
            (changed_envelope, existing(first), Conflict::Message),
            (
                reused_receipt,
                Existing {
                    receipt: Some(first),
                    ..Existing::default()
                },
                Conflict::Receipt,
            ),
            (
                reused_sequence,
                Existing {
                    sequence: Some(first),
                    ..Existing::default()
                },
                Conflict::Sequence,
            ),
            (receipt(2, "7"), Existing::default(), Conflict::OverOrdered),
        ] {
            let mut projection = Projection::new(line(), "12").unwrap();
            stage_and_apply(&mut projection, first);
            assert_eq!(projection.stage(fact, found), Admission::Conflict(reason));
            assert_eq!((projection.received, projection.through), (6, 1));
            assert_eq!(projection.observation(), Observation::Unknown);
            assert_eq!(projection.stage(first, existing(first)), Admission::Replay);
            assert_eq!(
                projection.stage(second, Existing::default()),
                Admission::Blocked
            );
            assert_eq!(projection.observation(), Observation::Unknown);
        }
    }

    #[test]
    fn over_ordered_staging_accounts_for_gaps_and_pending_facts() {
        let mut projection = Projection::new(line(), "12").unwrap();
        assert_eq!(
            projection.stage(receipt(2, "12"), Existing::default()),
            Admission::Conflict(Conflict::OverOrdered)
        );
        assert_eq!((projection.admitted_count, projection.through), (0, 0));
        assert_eq!(projection.observation(), Observation::Unknown);

        let mut projection = Projection::new(line(), "12").unwrap();
        assert_eq!(
            projection.stage(receipt(2, "7"), Existing::default()),
            Admission::Staged
        );
        assert_eq!(
            projection.stage(receipt(1, "6"), Existing::default()),
            Admission::Conflict(Conflict::OverOrdered)
        );
        assert_eq!(
            (
                projection.admitted_pieces,
                projection.received,
                projection.through
            ),
            (7, 0, 0)
        );
        assert_eq!(projection.observation(), Observation::Unknown);
    }

    #[test]
    fn foreign_scope_or_unrelated_tuple_cannot_stage_or_block_this_line() {
        let mut projection = Projection::new(line(), "12").unwrap();
        let first = receipt(1, "6");
        stage_and_apply(&mut projection, first);
        let unrelated_id = Uuid::from_u128(99);
        let [
            mut tenant,
            mut org,
            mut plan,
            mut order,
            mut purchase_line,
            mut item,
            mut rev,
        ] = [line(); 7];
        tenant.tenant_id = unrelated_id;
        org.organization_id = unrelated_id;
        plan.plan_id = unrelated_id;
        order.purchase_order_id = unrelated_id;
        purchase_line.purchase_line_id = unrelated_id;
        item.sku = "NM-MOTOR";
        rev.revision = None;

        for other in [tenant, org, plan, order, purchase_line, item, rev] {
            let other = VerifiedReceipt::from_verified(
                scope(other),
                other,
                first.id,
                "7",
                first.owner_content,
            )
            .ok()
            .unwrap();
            assert_eq!(
                projection.stage(other, existing(first)),
                Admission::NotForLine
            );
            assert_eq!(projection.apply_next(other), Err(ApplyFailure::NotForLine));
            assert_eq!(projection.observation(), observed(6, 6, 1));
        }
    }

    #[test]
    fn canonical_pieces_scope_unit_and_owner_identity_are_required() {
        let first = receipt(1, "6");
        let check = |scope, key, id, pieces| {
            VerifiedReceipt::from_verified(scope, key, id, pieces, first.owner_content).err()
        };
        let authorized = scope(line());
        for value in ["", "0", "06", "+6", "6.0", "6\n", "６", "1000001"] {
            assert_eq!(Projection::new(line(), value).err(), Some(Invalid::Pieces));
            assert_eq!(
                check(authorized, line(), first.id, value),
                Some(Invalid::Pieces)
            );
        }
        assert_eq!(receipt(1, "1000000").pieces, 1_000_000);
        let mut wrong_unit = line();
        wrong_unit.unit = "KG";
        assert_eq!(
            check(authorized, wrong_unit, first.id, "6"),
            Some(Invalid::Line)
        );
        for authorized_elsewhere in [
            Scope {
                tenant_id: Uuid::from_u128(99),
                ..authorized
            },
            Scope {
                organization_id: Uuid::from_u128(99),
                ..authorized
            },
        ] {
            assert_eq!(
                check(authorized_elsewhere, line(), first.id, "6"),
                Some(Invalid::Scope)
            );
        }
        let [
            mut zero,
            mut too_large,
            mut receipt,
            mut message,
            mut same_id,
        ] = [first.id; 5];
        zero.sequence = 0;
        too_large.sequence = 1_000_001;
        receipt.receipt_id = Uuid::nil();
        message.message_id = Uuid::nil();
        same_id.message_id = first.id.receipt_id;
        for id in [zero, too_large, receipt, message, same_id] {
            assert_eq!(check(authorized, line(), id, "6"), Some(Invalid::Identity));
        }
    }
}
