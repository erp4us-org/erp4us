use super::*;

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

const SCOPE: Scope = Scope {
    tenant: Uuid::from_u128(1),
    organization: Uuid::from_u128(2),
    actor: Uuid::from_u128(3),
};
const PURCHASE: PurchaseLine = PurchaseLine {
    order: Uuid::from_u128(6),
    line: Uuid::from_u128(7),
};

fn command(pieces: &str) -> Result<Command, Reject> {
    Command::new(SCOPE, id(4), id(5), PURCHASE, id(8), pieces)
}

fn next_command(pieces: &str) -> Command {
    Command::new(SCOPE, id(4), id(5), PURCHASE, id(11), pieces).unwrap()
}

struct Example {
    cmd: Command,
    plan: PlanRow,
}

impl Example {
    fn new() -> Self {
        let cmd = command("6").unwrap();
        Self {
            cmd,
            plan: PlanRow {
                key: PlanRowKey {
                    sales: cmd.sales,
                    plan: id(9),
                    stock: StockKey {
                        scope: cmd.sales.scope,
                        warehouse: id(10),
                        item: Item {
                            sku: "NM-ENDPLATE".into(),
                            revision: None,
                        },
                    },
                },
                original: OriginalBuyRow {
                    requested: 20,
                    held: 8,
                    buy: 12,
                },
            },
        }
    }

    fn issued(&self) -> IssuedLine<'_> {
        IssuedLine {
            scope: self.cmd.sales.scope,
            actor: self.cmd.actor,
            plan: self.plan.key.plan,
            purchase: self.cmd.purchase,
            item: &self.plan.key.stock.item,
            unit: "EA",
            ordered: 12,
        }
    }

    fn current(&self, previous: Option<&ReceiptEffect>) -> CurrentRow {
        CurrentRow {
            key: self.plan.key.clone(),
            eligibility: Eligibility::Waiting,
            purchase: previous.map(|effect| effect.line.binding.purchase),
            held: previous.map_or(self.plan.original.held, |effect| effect.plan_held),
        }
    }

    fn post(
        &self,
        cmd: Command,
        current: &CurrentRow,
        line: Option<&LineState>,
    ) -> Result<ReceiptEffect, Reject> {
        post_new(
            cmd,
            &self.plan,
            current,
            &self.issued(),
            line,
            Some(&self.plan.key.stock),
        )
    }
}

#[test]
fn end_plates_keep_the_original_snapshot_and_hold_the_entire_physical_lot() {
    let ex = Example::new();
    let original_plan = ex.plan.clone();
    let first = ex.post(ex.cmd, &ex.current(None), None).unwrap();
    let second = ex
        .post(
            next_command("6"),
            &ex.current(Some(&first)),
            Some(&first.line),
        )
        .unwrap();

    let mut physical = 8;
    let mut held = ex.plan.original.held;
    for (effect, sequence, allocated, plan_held, open) in
        [(&first, 1, 6, 14, 6), (&second, 2, 12, 20, 0)]
    {
        assert_eq!(effect.line.sequence, sequence);
        assert_eq!(effect.line.allocated, allocated);
        assert_eq!(effect.plan_held, plan_held);
        assert_eq!(effect.line_open, open);
        assert_eq!(effect.plan_remaining_shortage, open);
        assert_eq!(effect.physical_increase(), 6);
        assert_eq!(effect.hold_increase(), 6);
        physical += effect.physical_increase();
        held += effect.hold_increase();
        assert_eq!(held, plan_held);
        assert_eq!(physical - held, 0); // No part of the lot becomes free stock.
        assert_eq!(effect.line.binding.row, original_plan);
        assert_eq!(effect.line.binding.purchase, PURCHASE);
    }
    assert_eq!(first.line.binding, second.line.binding);
    assert_eq!(ex.plan, original_plan);
    assert_eq!(ex.plan.original.held, 8);
    assert_eq!(ex.plan.original.buy, 12);

    let mut exhausted = ex.current(Some(&second));
    exhausted.eligibility = Eligibility::Closed;
    // The thirteenth plate is an over-receipt, not a closed-plan conflict.
    assert_eq!(
        ex.post(
            Command {
                receipt_id: id(12),
                ..next_command("1")
            },
            &exhausted,
            Some(&second.line)
        ),
        Err(Reject::OverReceipt)
    );
}

#[test]
fn a_lot_that_does_not_fit_is_rejected_whole_even_if_some_pieces_would_fit() {
    let ex = Example::new();
    let first = ex.post(ex.cmd, &ex.current(None), None).unwrap();
    let current = ex.current(Some(&first));
    assert_eq!(
        ex.post(next_command("7"), &current, Some(&first.line)),
        Err(Reject::OverReceipt)
    );
    assert_eq!(current.held, 14);
    assert_eq!(first.line.allocated, 6);
    assert_eq!(first.line.sequence, 1);
}

#[test]
fn closed_released_or_incompatible_rows_cannot_receive_even_with_room() {
    let ex = Example::new();
    for eligibility in [
        Eligibility::Closed,
        Eligibility::Released,
        Eligibility::Incompatible,
    ] {
        let mut current = ex.current(None);
        current.eligibility = eligibility;
        assert_eq!(ex.post(ex.cmd, &current, None), Err(Reject::PlanIneligible));
    }
}

#[test]
fn lookup_requires_both_scope_fields_both_sales_ids_and_both_purchase_ids() {
    let ex = Example::new();
    for field in [
        |cmd: &mut Command| cmd.sales.scope.tenant = id(99),
        |cmd: &mut Command| cmd.sales.scope.organization = id(99),
        |cmd: &mut Command| cmd.sales.order = id(99),
        |cmd: &mut Command| cmd.sales.line = id(99),
        |cmd: &mut Command| cmd.purchase.order = id(99),
        |cmd: &mut Command| cmd.purchase.line = id(99),
    ] {
        let mut cmd = ex.cmd;
        field(&mut cmd);
        assert_eq!(ex.post(cmd, &ex.current(None), None), Err(Reject::NotFound));
    }
    let mut foreign = ex.issued();
    foreign.scope.organization = id(99);
    assert_eq!(
        post_new(
            ex.cmd,
            &ex.plan,
            &ex.current(None),
            &foreign,
            None,
            Some(&ex.plan.key.stock),
        ),
        Err(Reject::NotFound)
    );
}

#[test]
fn source_must_be_for_the_same_actor_plan_and_exact_positive_ea_material_row() {
    let ex = Example::new();
    let mut wrong_sku = ex.plan.key.stock.item.clone();
    wrong_sku.sku = "NM-SCREW".into();
    let mut wrong_revision = ex.plan.key.stock.item.clone();
    wrong_revision.revision = Some("A".into()); // Not the explicit null pin.
    let source = ex.issued();
    use Reject::{InvalidSource, PlanIneligible};
    for (issued, error) in [
        (
            IssuedLine {
                actor: id(99),
                ..source.clone()
            },
            InvalidSource,
        ),
        (
            IssuedLine {
                plan: id(99),
                ..source.clone()
            },
            InvalidSource,
        ),
        (
            IssuedLine {
                unit: "KG",
                ..source.clone()
            },
            InvalidSource,
        ),
        (
            IssuedLine {
                ordered: 0,
                ..source.clone()
            },
            InvalidSource,
        ),
        (
            IssuedLine {
                ordered: MAX_PIECES + 1,
                ..source.clone()
            },
            InvalidSource,
        ),
        (
            IssuedLine {
                item: &wrong_sku,
                ..source.clone()
            },
            PlanIneligible,
        ),
        (
            IssuedLine {
                item: &wrong_revision,
                ..source.clone()
            },
            PlanIneligible,
        ),
        (
            IssuedLine {
                ordered: 11,
                ..source
            },
            PlanIneligible,
        ),
    ] {
        assert_eq!(
            post_new(
                ex.cmd,
                &ex.plan,
                &ex.current(None),
                &issued,
                None,
                Some(&ex.plan.key.stock),
            ),
            Err(error)
        );
    }
}

#[test]
fn absent_or_different_stock_mappings_do_not_fall_back_to_a_warehouse_or_item() {
    let ex = Example::new();
    let stock = &ex.plan.key.stock;
    let another_warehouse = StockKey {
        warehouse: id(99),
        ..stock.clone()
    };
    let mut another_item = stock.clone();
    another_item.item.revision = Some("A".into());
    let mut foreign_stock = stock.clone();
    foreign_stock.scope.tenant = id(99);
    for destination in [
        None,
        Some(&another_warehouse),
        Some(&another_item),
        Some(&foreign_stock),
    ] {
        assert_eq!(
            post_new(
                ex.cmd,
                &ex.plan,
                &ex.current(None),
                &ex.issued(),
                None,
                destination
            ),
            Err(Reject::UnverifiedDestination)
        );
    }
}

#[test]
fn a_matching_second_issued_line_cannot_take_over_a_bound_row() {
    let ex = Example::new();
    let first = ex.post(ex.cmd, &ex.current(None), None).unwrap();
    for purchase in [
        PurchaseLine {
            order: id(99),
            ..PURCHASE
        },
        PurchaseLine {
            line: id(99),
            ..PURCHASE
        },
    ] {
        assert_eq!(
            post_new(
                Command {
                    purchase,
                    ..next_command("6")
                },
                &ex.plan,
                &ex.current(Some(&first)),
                &IssuedLine {
                    purchase,
                    ..ex.issued()
                },
                None,
                Some(&ex.plan.key.stock),
            ),
            Err(Reject::PlanIneligible)
        );
    }
}

#[test]
fn an_existing_line_cannot_be_rebound_to_different_sales_destination_or_original_hold() {
    let ex = Example::new();
    let first = ex.post(ex.cmd, &ex.current(None), None).unwrap();
    for field in [
        |line: &mut LineState| line.binding.row.key.sales.line = id(99),
        |line: &mut LineState| line.binding.row.key.stock.warehouse = id(99),
        |line: &mut LineState| {
            line.binding.row.original.requested += 1;
            line.binding.row.original.held += 1;
        },
    ] {
        let mut wrong_binding = first.line.clone();
        field(&mut wrong_binding);
        assert_eq!(
            ex.post(
                next_command("6"),
                &ex.current(Some(&first)),
                Some(&wrong_binding)
            ),
            Err(Reject::PlanIneligible)
        );
    }
}

#[test]
fn missing_binding_halves_and_incoherent_waiting_state_fail_closed() {
    let ex = Example::new();
    let first = ex.post(ex.cmd, &ex.current(None), None).unwrap();
    let current = ex.current(Some(&first));
    let cmd = next_command("6");
    assert_eq!(ex.post(cmd, &current, None), Err(Reject::InvalidState));
    assert_eq!(
        ex.post(cmd, &ex.current(None), Some(&first.line)),
        Err(Reject::InvalidState)
    );
    let mut wrong_row = ex.current(None);
    wrong_row.key.plan = id(99);
    assert_eq!(ex.post(ex.cmd, &wrong_row, None), Err(Reject::InvalidState));

    for (allocated, sequence) in [(0, 1), (13, 1), (6, 0), (6, 7), (6, i32::MAX)] {
        let invalid = LineState {
            allocated,
            sequence,
            ..first.line.clone()
        };
        assert_eq!(
            ex.post(cmd, &current, Some(&invalid)),
            Err(Reject::InvalidState)
        );
    }
    for held in [-1, 13, 15, 21, i32::MAX] {
        let invalid = CurrentRow {
            held,
            ..ex.current(Some(&first))
        };
        assert_eq!(
            ex.post(cmd, &invalid, Some(&first.line)),
            Err(Reject::InvalidState)
        );
    }
}

#[test]
fn original_row_requires_consistent_nonnegative_hold_and_a_positive_shortage() {
    let mut ex = Example::new();
    for (requested, held, buy, error) in [
        (20, -1, 21, Reject::InvalidState),
        (20, 7, 12, Reject::InvalidState),
        (20, 21, -1, Reject::InvalidState),
        (20, 20, 0, Reject::PlanIneligible),
        (i32::MAX, i32::MAX, 12, Reject::InvalidState),
        (MAX_PIECES + 1, MAX_PIECES, 1, Reject::InvalidState),
    ] {
        ex.plan.original = OriginalBuyRow {
            requested,
            held,
            buy,
        };
        assert_eq!(ex.post(ex.cmd, &ex.current(None), None), Err(error));
    }
}

#[test]
fn command_pieces_are_canonical_and_the_inclusive_ceiling_is_supported() {
    for pieces in [
        "",
        "0",
        "01",
        "-1",
        "+1",
        " 1",
        "1\n",
        "1.0",
        "1e0",
        "６",
        "1000001",
        "4294967296",
    ] {
        assert_eq!(command(pieces), Err(Reject::InvalidCommand), "{pieces:?}");
    }
    assert_eq!(command("1").unwrap().pieces, 1);
    assert_eq!(
        Command::new(SCOPE, id(4), id(5), PURCHASE, Uuid::nil(), "1"),
        Err(Reject::InvalidCommand)
    );

    let mut ex = Example::new();
    ex.plan.original = OriginalBuyRow {
        requested: MAX_PIECES,
        held: 0,
        buy: MAX_PIECES,
    };
    let source = IssuedLine {
        ordered: MAX_PIECES,
        ..ex.issued()
    };
    let effect = post_new(
        command("1000000").unwrap(),
        &ex.plan,
        &ex.current(None),
        &source,
        None,
        Some(&ex.plan.key.stock),
    )
    .unwrap();
    assert_eq!(effect.physical_increase(), MAX_PIECES);
    assert_eq!(effect.plan_held, MAX_PIECES);
    assert_eq!(effect.plan_remaining_shortage, 0);
    assert_eq!(effect.line_open, 0);
    let prior = LineState {
        allocated: MAX_PIECES - 1,
        sequence: MAX_PIECES - 1,
        ..effect.line
    };
    let current = CurrentRow {
        purchase: Some(PURCHASE),
        held: MAX_PIECES - 1,
        ..ex.current(None)
    };
    let last = post_new(
        next_command("1"),
        &ex.plan,
        &current,
        &source,
        Some(&prior),
        Some(&ex.plan.key.stock),
    )
    .unwrap();
    assert_eq!(last.line.sequence, MAX_PIECES);
    assert_eq!(last.line.allocated, MAX_PIECES);
}

#[test]
fn replay_returns_the_original_actor_snapshot_without_a_new_receipt_effect() {
    let ex = Example::new();
    assert_eq!(check_replay(ex.cmd, None), Ok(None));
    let first = ex.post(ex.cmd, &ex.current(None), None).unwrap();
    let second = ex
        .post(
            next_command("6"),
            &ex.current(Some(&first)),
            Some(&first.line),
        )
        .unwrap();
    let replay = check_replay(ex.cmd, Some(&first)).unwrap().unwrap();
    assert!(std::ptr::eq(replay, &first));
    assert_eq!(replay.plan_held, 14);
    assert_eq!(second.plan_held, 20);
}

#[test]
fn same_id_with_changed_intent_or_actor_does_not_return_the_saved_receipt() {
    let ex = Example::new();
    let first = ex.post(ex.cmd, &ex.current(None), None).unwrap();
    for field in [
        |cmd: &mut Command| cmd.sales.order = id(99),
        |cmd: &mut Command| cmd.sales.line = id(99),
        |cmd: &mut Command| cmd.purchase.order = id(99),
        |cmd: &mut Command| cmd.purchase.line = id(99),
        |cmd: &mut Command| cmd.pieces += 1,
    ] {
        let mut changed = ex.cmd;
        field(&mut changed);
        assert_eq!(
            check_replay(changed, Some(&first)),
            Err(Reject::IntentChanged)
        );
    }
    let another_actor = Command {
        actor: id(99),
        pieces: 7,
        ..ex.cmd
    };
    assert_eq!(
        check_replay(another_actor, Some(&first)),
        Err(Reject::ActorMismatch)
    );
    for field in [
        |cmd: &mut Command| cmd.sales.scope.tenant = id(99),
        |cmd: &mut Command| cmd.sales.scope.organization = id(99),
        |cmd: &mut Command| cmd.receipt_id = id(99),
    ] {
        let mut key = ex.cmd;
        field(&mut key);
        assert_ne!(key.replay_key(), ex.cmd.replay_key());
        assert_eq!(check_replay(key, Some(&first)), Err(Reject::InvalidState));
    }
}
