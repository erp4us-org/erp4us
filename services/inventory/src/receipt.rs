//! Pure decision for the proposed waiting-plan receipt. Authorization, owner
//! verification and locking/atomic commit of the complete effect belong to the
//! caller. No part of the original plan or its netting fact is updated.

use crate::netting::{Item, MAX_PIECES, positive};
use erp4us_identity::Scope;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Organization {
    tenant: Uuid,
    organization: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SalesLine {
    scope: Organization,
    order: Uuid,
    line: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PurchaseLine {
    order: Uuid,
    line: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Command {
    sales: SalesLine,
    purchase: PurchaseLine,
    receipt_id: Uuid,
    actor: Uuid,
    pieces: i32,
}

impl Command {
    /// `scope` comes from current Inventory authorization, not the request
    /// body. The caller decodes canonical UUIDs before constructing the command.
    fn new(
        scope: Scope,
        order: Uuid,
        line: Uuid,
        purchase: PurchaseLine,
        receipt_id: Uuid,
        pieces: &str,
    ) -> Result<Self, Reject> {
        if [
            scope.tenant,
            scope.organization,
            scope.actor,
            order,
            line,
            purchase.order,
            purchase.line,
            receipt_id,
        ]
        .iter()
        .any(Uuid::is_nil)
        {
            return Err(Reject::InvalidCommand);
        }
        Ok(Self {
            sales: SalesLine {
                scope: Organization {
                    tenant: scope.tenant,
                    organization: scope.organization,
                },
                order,
                line,
            },
            purchase,
            receipt_id,
            actor: scope.actor,
            pieces: positive(pieces).ok_or(Reject::InvalidCommand)?,
        })
    }

    fn replay_key(self) -> (Organization, Uuid) {
        (self.sales.scope, self.receipt_id)
    }
}

/// Selected tuple of the authenticated Procurement source. The caller must
/// verify the owner, same authorized actor and scope, response shape (including
/// explicit revision), and uniqueness of the purchase pair before passing it in.
#[derive(Clone)]
struct IssuedLine<'a> {
    scope: Organization,
    actor: Uuid,
    plan: Uuid,
    purchase: PurchaseLine,
    item: &'a Item,
    unit: &'a str,
    ordered: i32,
}

/// An EA component stock key. The caller verifies it against Inventory stock;
/// this is not a destination supplied by the receiver or by Procurement.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StockKey {
    scope: Organization,
    warehouse: Uuid,
    item: Item,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlanRowKey {
    sales: SalesLine,
    plan: Uuid,
    stock: StockKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OriginalBuyRow {
    requested: i32,
    held: i32,
    buy: i32,
}

/// Captured from the immutable Inventory plan's EA material row, never from a
/// current availability lookup. The adapter must check the stored unit before
/// constructing this snapshot. The binding pins the original row values.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PlanRow {
    key: PlanRowKey,
    original: OriginalBuyRow,
}

#[derive(Clone, Copy)]
enum Eligibility {
    Waiting,
    Closed,
    Released,
    Incompatible,
}

struct CurrentRow {
    key: PlanRowKey,
    // Effective eligibility must account for the plan as well as the row.
    eligibility: Eligibility,
    purchase: Option<PurchaseLine>,
    held: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    row: PlanRow,
    purchase: PurchaseLine,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LineState {
    binding: Binding,
    allocated: i32,
    sequence: i32,
}

/// One decision for the complete receipt: a physical movement, its full row
/// hold, the new line balance/sequence and the local allocation after the commit.
/// The command and this snapshot also supply the immutable replay result; the
/// caller adds the posting time. There is no change to free stock.
#[derive(Debug, PartialEq, Eq)]
struct ReceiptEffect {
    command: Command,
    line: LineState,
    line_open: i32,
    plan_held: i32,
    plan_remaining_shortage: i32,
}

impl ReceiptEffect {
    fn physical_increase(&self) -> i32 {
        self.command.pieces
    }

    fn hold_increase(&self) -> i32 {
        self.command.pieces
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Reject {
    InvalidCommand,
    NotFound,
    InvalidSource,
    UnverifiedDestination,
    InvalidState,
    OverReceipt,
    PlanIneligible,
    ActorMismatch,
    IntentChanged,
}

/// Call after current Inventory authorization, before any new-command lookups,
/// and recheck under the write guard. Scope is part of the lookup key, not of
/// the intent comparison; an actor mismatch must not disclose the record.
fn check_replay(
    command: Command,
    saved: Option<&ReceiptEffect>,
) -> Result<Option<&ReceiptEffect>, Reject> {
    let Some(saved) = saved else {
        return Ok(None);
    };
    if saved.command.replay_key() != command.replay_key() {
        return Err(Reject::InvalidState);
    }
    if saved.command.actor != command.actor {
        return Err(Reject::ActorMismatch);
    }
    if saved.command != command {
        return Err(Reject::IntentChanged);
    }
    Ok(Some(saved))
}

/// Decide only after the replay lookup found no command. The caller obtains
/// the live hold, binding and balance in the same guarded transaction that will
/// commit the effect; absence is not a substitute for an unreadable record.
fn post_new(
    command: Command,
    plan: &PlanRow,
    current: &CurrentRow,
    issued: &IssuedLine<'_>,
    line: Option<&LineState>,
    verified_stock: Option<&StockKey>,
) -> Result<ReceiptEffect, Reject> {
    if plan.key.sales != command.sales
        || issued.scope != command.sales.scope
        || issued.purchase != command.purchase
    {
        return Err(Reject::NotFound);
    }
    if issued.actor != command.actor
        || issued.plan.is_nil()
        || issued.plan != plan.key.plan
        || issued.unit != "EA"
        || !issued.item.valid()
        || !(1..=MAX_PIECES).contains(&issued.ordered)
    {
        return Err(Reject::InvalidSource);
    }
    if current.key != plan.key {
        return Err(Reject::InvalidState);
    }

    let (allocated, sequence) = match line {
        Some(line) => {
            let bound = &line.binding.row;
            if bound.key.sales.scope != issued.scope
                || bound.key.plan != issued.plan
                || line.binding.purchase != issued.purchase
                || &bound.key.stock.item != issued.item
                || bound.original.buy != issued.ordered
                || !(1..=issued.ordered).contains(&line.allocated)
                || !(1..=line.allocated).contains(&line.sequence)
            {
                return Err(Reject::InvalidState);
            }
            (line.allocated, line.sequence)
        }
        // A missing balance for an already-bound line is not a fresh line.
        None if current.purchase == Some(command.purchase) => return Err(Reject::InvalidState),
        None => (0, 0),
    };
    let open = subtract(issued.ordered, allocated).ok_or(Reject::InvalidState)?;
    if command.pieces > open {
        // The balance of the verified purchase line takes precedence over a
        // closed or exhausted plan row. Do not cap a lot to the remaining stock.
        return Err(Reject::OverReceipt);
    }

    let original = plan.original;
    if add(original.held, original.buy) != Some(original.requested) {
        return Err(Reject::InvalidState);
    }
    if !matches!(current.eligibility, Eligibility::Waiting)
        || original.buy == 0
        || original.buy != issued.ordered
        || &plan.key.stock.item != issued.item
        || current
            .purchase
            .is_some_and(|bound| bound != command.purchase)
    {
        return Err(Reject::PlanIneligible);
    }
    if plan.key.stock.scope != command.sales.scope
        || plan.key.stock.warehouse.is_nil()
        || verified_stock != Some(&plan.key.stock)
    {
        return Err(Reject::UnverifiedDestination);
    }
    let binding = Binding {
        row: plan.clone(),
        purchase: command.purchase,
    };
    match (current.purchase, line) {
        (None, None) => {}
        (Some(_), Some(line)) if line.binding == binding => {}
        (Some(_), Some(_)) => return Err(Reject::PlanIneligible),
        _ => return Err(Reject::InvalidState),
    }
    if add(original.held, allocated) != Some(current.held) {
        return Err(Reject::InvalidState);
    }
    let room = subtract(original.requested, current.held).ok_or(Reject::InvalidState)?;
    let remaining = subtract(room, command.pieces).ok_or(Reject::PlanIneligible)?;

    Ok(ReceiptEffect {
        command,
        line: LineState {
            binding,
            allocated: add(allocated, command.pieces).ok_or(Reject::InvalidState)?,
            sequence: add(sequence, 1).ok_or(Reject::InvalidState)?,
        },
        line_open: subtract(open, command.pieces).ok_or(Reject::InvalidState)?,
        plan_held: add(current.held, command.pieces).ok_or(Reject::InvalidState)?,
        plan_remaining_shortage: remaining,
    })
}

fn add(left: i32, right: i32) -> Option<i32> {
    if left < 0 || right < 0 {
        return None;
    }
    left.checked_add(right).filter(|sum| *sum <= MAX_PIECES)
}

fn subtract(left: i32, right: i32) -> Option<i32> {
    left.checked_sub(right)
        .filter(|difference| (0..=MAX_PIECES).contains(difference))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod mongo;
#[cfg(test)]
mod source;
