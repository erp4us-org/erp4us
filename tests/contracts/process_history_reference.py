"""Local data proof for the proposed #22 NovaMotion purchase-draft history.

Run with python3 tests/contracts/process_history_reference.py. This proves only
the shortage line in the two published JSON examples. Three Python-only owner
facts and the caught-up measurements are invented test inputs, not contracts
or evidence of publication. The narrow decoder is not a JSON Schema validator
or a header check (the examples contain envelopes only).

Neither matching scope nor envelope metadata proves producer provenance or
caller access; the fixture assumes those checks happened elsewhere. A new
in-memory replay does not prove durable writes, ACK recovery or retention.
Fresh-ID replay follows the original in this proof; matching content alone
cannot establish which ID was original on an alias-first rebuild.
No draft in this view is not evidence that Procurement did not act. The
synthetic freshness bound does not establish real filter/durable coverage,
and the fixture says nothing about receipts, approval or other kinds of lines.
"""

import json
import unittest
from dataclasses import dataclass, replace
from hashlib import sha256
from pathlib import Path


EXAMPLES = Path(__file__).resolve().parents[2] / "contracts/examples/nats"
ORDER, RESERVATION, PLAN, HANDOFF, DRAFT = STEPS = (
    "order_confirmed", "reservation_recorded", "material_plan_saved",
    "handoff_observed", "draft_recorded",
)
NEXT = ("order", "reservation", "plan", "handoff", "draft")
STALL_AFTER = 30  # RFC preview proposal, not a deployment setting
FRESH_FOR = 5  # Test-only measurement bound, not an RFC decision


@dataclass(frozen=True)
class LineKey:
    tenant: str
    organization: str
    order: str
    line: str


@dataclass(frozen=True)
class Fact:
    kind: str
    key: LineKey
    message_id: str
    correlation_id: str
    parent_id: str | None
    occurred_at: str
    revision: int | None = None
    plan: str | None = None
    proposal: str | None = None
    outcome: str | None = None
    has_buy_rows: bool | None = None
    snapshot_digest: str = ""


def published_fact(envelope, kind):
    """Read just the two example shapes, retaining no BOM or stock payload."""
    owner, route = {
        ORDER: ("sales", "order.confirmed"),
        HANDOFF: ("inventory", "material-plan.netted"),
    }[kind]
    tenant = envelope["tenantId"]
    if (
        envelope["messageType"] != f"v1.business.{tenant}.{owner}.event.{route}.v1"
        or envelope["sourceService"] != f"erp4us.{owner}"
        or (envelope["protocolVersion"], envelope["schemaVersion"]) != ("1.0.0", "1.0.0")
    ):
        raise ValueError("unexpected route, source field or version in example")

    payload = envelope["payload"]
    # Compare known v1 business snapshot fields, not payload bytes, actor or
    # transport metadata. The wire contract already requires canonical buy row order.
    if kind == ORDER:
        snapshot = [payload[field] for field in ("sku", "productRevision", "unit", "pieces")]
        fields = {"revision": payload["revision"]}
    else:
        bom, rows = payload["bom"], payload["buyRows"]
        snapshot = [bom["sku"], bom["revision"],
                    [[row[f] for f in ("sku", "revision", "unit", "pieces")] for row in rows]]
        fields = {"plan": payload["planId"], "has_buy_rows": bool(rows)}
    digest = sha256(json.dumps(snapshot, separators=(",", ":")).encode("ascii")).hexdigest()
    return Fact(
        kind, LineKey(tenant, envelope["organizationId"], payload["orderId"], payload["lineId"]),
        envelope["messageId"], envelope["correlationId"], envelope.get("causationId"),
        envelope["timestamp"], snapshot_digest=digest, **fields,
    )


@dataclass(frozen=True)
class CaughtUpSample:
    observed_at: int  # Seconds on the local test clock, not owner event time.
    pending: int | None = 0
    ack_pending: int | None = 0
    exact_reader: bool = True

    def proves_caught_up(self, now):
        return (self.exact_reader and 0 <= now - self.observed_at <= FRESH_FOR
                and self.pending == self.ack_pending == 0)


class NovaMotionLine:
    """One fixed five-step line, not a general projector or an API."""

    def __init__(self, key):
        self.key = key
        self.by_id = {}
        self.first = {}  # One logical occurrence per step for this immutable plan.
        self.conflicts = set()
        self.handoff_seen = None
        self.sample = None

    def deliver(self, fact, now):
        if fact.key != self.key:
            return "outside_line"
        if fact.kind == ORDER and (fact.correlation_id != fact.message_id or fact.parent_id):
            return "invalid_root"
        original = self.by_id.get(fact.message_id)
        if original is not None:
            if original == fact:
                return "id_replay"
            self.conflicts.add(original.kind)
            return "conflict"
        first = self.first.get(fact.kind)
        if first is not None and replace(fact, message_id=first.message_id) != first:
            self.conflicts.add(fact.kind)
            return "conflict"
        self.by_id[fact.message_id] = fact
        if first is not None:
            return "logical_replay"
        self.first[fact.kind] = fact
        if fact.kind == HANDOFF:
            self.handoff_seen = now
        return "accepted"

    def disconnect(self):
        self.sample = None

    def read(self, authorized_key, now):
        if authorized_key != self.key or ORDER not in self.first or ORDER in self.conflicts:
            return None
        steps = []

        def result(state, reason):
            return {"state": state, "reason": reason, "steps": steps}

        root = self.first[ORDER]
        for index, kind in enumerate(STEPS):
            if kind in self.conflicts:
                return result("incomplete", "conflicting_fact")
            fact = self.first.get(kind)
            if fact is None:
                if any(later in self.first for later in STEPS[index + 1:]):
                    return result("incomplete", "missing_predecessor")
                if kind == DRAFT and now - self.handoff_seen >= STALL_AFTER:
                    if self.sample is not None and self.sample.proves_caught_up(now):
                        return result("stalled", "draft_not_observed")
                    return result("incomplete", "caught_up_unverified")
                return result("waiting", f"{NEXT[index]}_not_observed")
            if index:
                # Handoff's wire parent is Sales, not Inventory's saved plan.
                # The loop still requires the saved plan before either later step.
                parent = root if kind == HANDOFF else self.first[STEPS[index - 1]]
                if fact.correlation_id != root.message_id or fact.parent_id != parent.message_id:
                    return result("incomplete", "inconsistent_link")
            if kind in (HANDOFF, DRAFT):
                saved = self.first[PLAN]
                if fact.plan != saved.plan or not saved.has_buy_rows:
                    return result("incomplete", "inconsistent_plan")
            step = {"kind": kind, "occurred_at": fact.occurred_at}
            if kind == RESERVATION:
                step["outcome"] = fact.outcome
            steps.append(step)
        return result("draft_recorded", "draft_observed")


class ProcessHistoryReference(unittest.TestCase):
    def setUp(self):
        self.sales_wire = json.loads((EXAMPLES / "sales-order-confirmed-v1.json").read_text())
        self.handoff_wire = json.loads(
            (EXAMPLES / "inventory-material-plan-netted-v1.json").read_text()
        )
        self.order = published_fact(self.sales_wire, ORDER)
        self.handoff = published_fact(self.handoff_wire, HANDOFF)
        self.key = self.order.key
        root = self.order.message_id

        # Test-only assumptions about already committed owner facts. These are
        # Python values, not proposed wire envelopes or live owner outputs.
        self.reservation = Fact(RESERVATION, self.key, "test-reservation", root, root,
                                "2026-11-27T08:59:59Z", revision=1, outcome="none")
        self.plan = Fact(PLAN, self.key, "test-saved-plan", root, self.reservation.message_id,
                         "2026-11-28T08:00:01Z", revision=1, plan=self.handoff.plan,
                         has_buy_rows=True)
        self.draft = Fact(DRAFT, self.key, "test-draft", root, self.handoff.message_id,
                          "2026-11-28T07:59:59Z", revision=1, plan=self.plan.plan,
                          proposal="test-proposal")
        self.chain = (self.order, self.reservation, self.plan, self.handoff, self.draft)

    def build(self, *facts):
        history = NovaMotionLine(self.key)
        for fact in facts:
            self.assertEqual(history.deliver(fact, 100), "accepted")
        return history

    def expect(self, history, state, reason, count, *, now=130):
        view = history.read(self.key, now)
        self.assertEqual(view, {
            "state": state, "reason": reason, "steps": [
                {"kind": fact.kind, "occurred_at": fact.occurred_at,
                 **({"outcome": "none"} if fact.kind == RESERVATION else {})}
                for fact in self.chain[:count]
            ],
        })
        return view

    def test_reverse_delivery_and_skewed_clocks(self):
        self.assertEqual(self.handoff.key, self.key)
        self.assertEqual(self.handoff.parent_id, self.order.message_id)
        self.assertEqual(self.handoff.correlation_id, self.order.message_id)
        self.assertLess(self.reservation.occurred_at, self.order.occurred_at)
        self.assertLess(self.draft.occurred_at, self.handoff.occurred_at)
        reverse = tuple(reversed(self.chain))
        no_root = self.build(*reverse[:-1])
        self.assertIsNone(no_root.read(self.key, 130))
        self.assertEqual(no_root.deliver(self.order, 130), "accepted")
        view = self.expect(no_root, "draft_recorded", "draft_observed", 5)
        self.assertEqual(view, self.build(*self.chain).read(self.key, 130))

    def test_missing_parent_and_rebuild_from_retained_facts(self):
        for missing in (1, 2, 3):  # Reservation, typed plan prerequisite, wire handoff.
            with self.subTest(missing=STEPS[missing]):
                retained = self.chain[:missing] + self.chain[missing + 1:]
                for replay_order in (retained, tuple(reversed(retained))):
                    history = self.build(*replay_order)
                    history.sample = CaughtUpSample(130)
                    self.expect(history, "incomplete", "missing_predecessor", missing)
                    history.deliver(self.chain[missing], 130)
                    self.expect(history, "draft_recorded", "draft_observed", 5)

    def test_waiting_stalled_and_measurement_uncertainty(self):
        for count, name in enumerate(NEXT[1:4], start=1):
            self.expect(self.build(*self.chain[:count]), "waiting", f"{name}_not_observed",
                        count, now=1000)
        history = self.build(*self.chain[:-1])
        self.expect(history, "waiting", "draft_not_observed", 4, now=129)
        for sample in (None, CaughtUpSample(124), CaughtUpSample(131),
                       CaughtUpSample(130, pending=1), CaughtUpSample(130, ack_pending=1),
                       CaughtUpSample(130, pending=None), CaughtUpSample(130, exact_reader=False)):
            with self.subTest(sample=sample):
                history.sample = sample
                self.expect(history, "incomplete", "caught_up_unverified", 4)
        history.sample = CaughtUpSample(125)
        self.expect(history, "stalled", "draft_not_observed", 4)
        history.disconnect()
        self.expect(history, "incomplete", "caught_up_unverified", 4)
        history.deliver(self.draft, 131)
        self.expect(history, "draft_recorded", "draft_observed", 5, now=131)

    def test_replay_preserves_original_id_and_first_observation(self):
        history = self.build(*self.chain[:-1])
        reordered = json.loads(json.dumps(self.handoff_wire, sort_keys=True))
        self.assertEqual(history.deliver(published_fact(reordered, HANDOFF), 129), "id_replay")
        alias = published_fact({**reordered, "messageId": "test-equivalent-handoff"}, HANDOFF)
        self.assertEqual(history.deliver(alias, 129), "logical_replay")
        self.assertEqual(history.deliver(alias, 129), "id_replay")
        self.assertEqual(history.first[HANDOFF].message_id, self.handoff.message_id)
        history.sample = CaughtUpSample(130)
        self.expect(history, "stalled", "draft_not_observed", 4)
        history.deliver(self.draft, 130)
        self.expect(history, "draft_recorded", "draft_observed", 5)
        other = self.build(*self.chain[:-1])
        other.deliver(alias, 129)
        other.deliver(replace(self.draft, parent_id=alias.message_id), 130)
        self.expect(other, "incomplete", "inconsistent_link", 4)

    def test_conflicting_snapshot_freezes_branch_in_either_arrival_order(self):
        for message_id in (self.handoff.message_id, "test-conflicting-handoff"):
            wire = json.loads(json.dumps(self.handoff_wire))
            wire["messageId"] = message_id
            wire["payload"]["buyRows"][0]["pieces"] = "13"
            conflict = published_fact(wire, HANDOFF)
            for first, second in ((self.handoff, conflict), (conflict, self.handoff)):
                with self.subTest(message_id=message_id, first=first.snapshot_digest):
                    history = self.build(*self.chain[:3], first)
                    self.assertEqual(history.deliver(second, 101), "conflict")
                    history.deliver(self.draft, 102)
                    self.assertEqual(history.deliver(first, 103), "id_replay")
                    self.expect(history, "incomplete", "conflicting_fact", 3)

    def test_wrong_scope_or_line_does_not_fill_parent_gap_or_affect_valid_run(self):
        for field in ("tenant", "organization", "order", "line"):
            with self.subTest(field=field):
                wrong = replace(self.key, **{field: "test-foreign"})
                history = self.build(*self.chain[:2], self.handoff, self.draft)
                foreign = replace(self.plan, key=wrong)
                self.assertEqual(history.deliver(foreign, 101), "outside_line")
                self.assertIsNone(history.read(wrong, 130))
                self.expect(history, "incomplete", "missing_predecessor", 2)
                history.deliver(self.plan, 102)
                self.assertEqual(history.deliver(foreign, 103), "outside_line")
                self.expect(history, "draft_recorded", "draft_observed", 5)

    def test_wrong_link_and_unsupported_wire_version(self):
        for changes, reason in (
            ({"parent_id": self.plan.message_id}, "inconsistent_link"),
            ({"correlation_id": "test-foreign-root"}, "inconsistent_link"),
            ({"plan": "test-foreign-plan"}, "inconsistent_plan"),
        ):
            with self.subTest(changes=changes):
                history = self.build(*self.chain[:-1], replace(self.draft, **changes))
                self.expect(history, "incomplete", reason, 4)
        for field in ("protocolVersion", "schemaVersion"):
            with self.subTest(field=field), self.assertRaises(ValueError):
                published_fact({**self.handoff_wire, field: "1.1.0"}, HANDOFF)

    def test_view_allowlists_fields_and_does_not_mutate_history(self):
        history = self.build(*self.chain)
        # Exact keys and values exclude actor IDs, business IDs, wire metadata,
        # timestamps of local observations, BOM, quantities and full snapshots.
        view = self.expect(history, "draft_recorded", "draft_observed", 5)
        view["steps"][0]["occurred_at"] = "changed in caller"
        self.expect(history, "draft_recorded", "draft_observed", 5)


if __name__ == "__main__":
    unittest.main()
