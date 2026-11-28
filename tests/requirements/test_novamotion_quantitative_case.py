"""Arithmetic oracle for the proposed synthetic NovaMotion acceptance case.

This has no service, identity or persistence behavior. It checks the pinned seed,
published shortage example and expected balances before those are used as an
oracle by a future live receipt, production and delivery test.
"""
from collections import Counter
from decimal import Decimal
import json
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
FRAME = ("NM-1100", "A")
DRIVE = ("NM-1200", "A")
PRODUCT = ("NM-1000", "A")
PLATE = ("NM-ENDPLATE", None)
ORDER_PIECES = 10


def item(row):
    return row["sku"], row["revision"]


def fixture(path):
    return json.loads((ROOT / path).read_text())


class Reconciliation:
    """A single warehouse and a single scenario hold, not Inventory code."""

    def __init__(self):
        seed = fixture("services/manufacturing/src/novamotion.json")
        self.components = {item(row): row["components"] for row in seed["recipes"]}
        self.open = Counter({item(row): int(row["pieces"]) for row in fixture(
            "contracts/examples/nats/inventory-material-plan-netted-v1.json")["payload"]["buyRows"]})
        self.initial_hold = Counter({
            ("NM-PROFILE", None): 20, PLATE: 8, ("NM-SCREW", None): 120,
            ("NM-MOTOR", None): 4, ("NM-COUPLING", None): 10,
            ("NM-RAIL", None): 12, ("NM-SENSOR", None): 10,
            ("NM-CABLE", None): 10, ("NM-PACK", None): 10,
        })
        self.on_hand = self.initial_hold.copy()
        self.held = self.initial_hold.copy()
        self.posted = {}
        self.delivered = 0

    def inputs(self, output):
        return Counter({item(row): row["pieces"] * ORDER_PIECES for row in self.components[output]})

    def change(self, physical_delta, hold_delta):
        # Counter.update retains negative values for explicit checking; Counter
        # addition would silently discard any underflow.
        physical, held = self.on_hand.copy(), self.held.copy()
        physical.update(physical_delta)
        held.update(hold_delta)
        if any(not 0 <= held[key] <= physical[key] for key in physical.keys() | held.keys()):
            raise ValueError("insufficient held or physical stock")
        self.on_hand, self.held = physical, held

    def receive(self, receipt_id, key, pieces):
        intent = (key, pieces)
        if receipt_id in self.posted:
            if self.posted[receipt_id] != intent:
                raise ValueError("changed receipt intent")
            return
        if not 0 < pieces <= self.open[key]:
            raise ValueError("receipt exceeds issued balance")
        self.change({key: pieces}, {key: pieces})
        self.open[key] -= pieces
        self.posted[receipt_id] = intent

    def complete(self, output):
        delta = {key: -quantity for key, quantity in self.inputs(output).items()}
        delta[output] = ORDER_PIECES
        self.change(delta, delta)

    def dispatch(self, pieces):
        # Ship-complete is an explicit assumption of this example, not a
        # default delivery policy or approval to expose a shipping command.
        if pieces != ORDER_PIECES - self.delivered or pieces <= 0:
            raise ValueError("does not match the proposed ship-complete terms")
        self.change({PRODUCT: -pieces}, {PRODUCT: -pieces})
        self.delivered += pieces


class NovaMotionQuantitativeCase(unittest.TestCase):
    def test_preconditions_agree_with_the_pinned_sources(self):
        case = Reconciliation()
        needs = case.inputs(FRAME) + case.inputs(DRIVE) + case.inputs(PRODUCT)
        del needs[FRAME], needs[DRIVE]  # made internally, not a purchased leaf
        self.assertEqual(case.initial_hold + case.open, needs)
        self.assertEqual(dict(case.open), {
            PLATE: 12, ("NM-MOTOR", None): 6, ("NM-SPINDLE", None): 10,
            ("NM-RAIL", None): 8, ("NM-SENSOR", None): 10,
        })
        product = next(row for row in fixture("services/manufacturing/src/novamotion.json")["products"]
                       if item(row) == PRODUCT)
        self.assertEqual(Decimal(product["price_eur"]) * ORDER_PIECES, Decimal("24900.00"))

    def test_partial_end_plate_receipt_blocks_only_the_affected_branch(self):
        case = Reconciliation()
        self.assertEqual(case.held[PLATE], 8)
        case.receive("first-plate-lot", PLATE, 6)
        self.assertEqual((case.held[PLATE], case.open[PLATE]), (14, 6))
        case.receive("first-plate-lot", PLATE, 6)
        with self.assertRaisesRegex(ValueError, "changed receipt"):
            case.receive("first-plate-lot", PLATE, 5)
        self.assertEqual((case.held[PLATE], case.open[PLATE]), (14, 6))
        with self.assertRaisesRegex(ValueError, "exceeds issued"):
            case.receive("oversize", PLATE, 7)
        for key, pieces in tuple(case.open.items()):
            if key != PLATE:
                case.receive(key[0], key, pieces)

        before = case.on_hand.copy(), case.held.copy()
        with self.assertRaisesRegex(ValueError, "insufficient"):
            case.complete(FRAME)
        self.assertEqual((case.on_hand, case.held), before)
        case.complete(DRIVE)
        self.assertEqual(case.on_hand[DRIVE], 10)
        self.assertEqual(case.on_hand[DRIVE] - case.held[DRIVE], 0)
        with self.assertRaisesRegex(ValueError, "insufficient"):
            case.complete(PRODUCT)
        self.assertEqual(case.on_hand[PRODUCT], 0)

    def test_complete_case_balances_scenario_items_to_zero(self):
        case = Reconciliation()
        for key, pieces in tuple(case.open.items()):
            if key == PLATE:
                case.receive("first-plate-lot", key, 6)
                case.receive("second-plate-lot", key, 6)
            else:
                case.receive(key[0], key, pieces)
        self.assertEqual((case.held[PLATE], case.open[PLATE]), (20, 0))
        for output in (FRAME, DRIVE, PRODUCT):
            case.complete(output)
            self.assertEqual((case.on_hand[output], case.held[output]), (10, 10))
        with self.assertRaisesRegex(ValueError, "ship-complete"):
            case.dispatch(6)
        self.assertEqual((case.on_hand[PRODUCT], case.held[PRODUCT], case.delivered), (10, 10, 0))
        case.dispatch(10)
        self.assertEqual(ORDER_PIECES - case.delivered, 0)
        self.assertTrue(all(value == 0 for value in case.open.values()))
        self.assertTrue(all(value == 0 for value in case.held.values()))
        self.assertTrue(all(value == 0 for value in case.on_hand.values()))
