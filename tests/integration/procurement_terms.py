"""Prove exact editable draft terms without supplier commitments."""
from concurrent.futures import ThreadPoolExecutor
from copy import deepcopy
from decimal import Decimal
import json
import os
from uuid import uuid4

from identity_fixture import EXTERNAL_ORGANIZATION, OTHER_ORGANIZATION, OTHER_TENANT, TENANT, token
from procurement_handoff import PROCUREMENT, manage
from smoke import request, wait_until_ready


PRICES = {"NM-ENDPLATE": "30.10", "NM-MOTOR": "149.95", "NM-SPINDLE": "45.20",
          "NM-RAIL": "12.00", "NM-SENSOR": "7.35"}


def exercise(proposal):
    if os.getenv("COMPOSE_PROJECT_NAME") in (None, "", "erp4us", "erp4us-preview"):
        raise RuntimeError("Purchase terms tests require an isolated, named Compose project")
    path = f"/v1/procurement/proposals/by-plan/{proposal['plan_id']}/terms"
    observer = token(subject="synthetic-procurement-observer")

    def terms(command=None, **kwargs):
        data = json.dumps(command).encode() if command is not None else None
        return request(path, data, origin=PROCUREMENT, timeout=11, **kwargs)

    assert terms()[0] == 404
    assert terms(bearer=None)[0] == 401
    status, _, partners = request("/v1/partners?role=supplier")
    assert status == 200
    suppliers = {row["code"]: row for row in partners["items"]}
    supplier = suppliers["SUP-VECTORDRIVE"]
    command = {"command_id": str(uuid4()), "expected_revision": 0, "lines": [
        {"sku": row["sku"], "revision": row["revision"],
         "supplier_id": supplier["id"], "unit_price": PRICES[row["sku"]]} for row in proposal["lines"]
    ]}
    assert terms(command, bearer=observer)[0] == 403
    invalid = deepcopy(command)
    invalid["lines"][0]["unit_price"] = "30.100"
    assert terms(invalid)[0] == 400
    invalid = deepcopy(command)
    invalid["lines"].pop()
    assert terms(invalid)[0] == 400
    status, _, customers = request("/v1/partners?role=customer")
    assert status == 200
    invalid = deepcopy(command)
    invalid["lines"][0]["supplier_id"] = next(row["id"] for row in customers["items"] if row["code"] == "CUS-ATLAS")
    assert terms(invalid)[0] == 400
    status, _, foreign_supplier = request("/v1/partners:register", json.dumps({
        "code": f"SUP-OTHER-{uuid4().hex[:12].upper()}",
        "name": "Other organization supplier",
        "roles": ["supplier"],
    }).encode(), bearer=token(subject="synthetic-foreign-supplier-writer"), scope=(TENANT, OTHER_ORGANIZATION))
    assert status == 201
    invalid["lines"][0]["supplier_id"] = foreign_supplier["id"]
    assert terms(invalid)[0] == 400
    assert terms()[0] == 404

    ordered_differently = {**command, "lines": list(reversed(command["lines"]))}
    with ThreadPoolExecutor(max_workers=2) as pool:
        responses = list(pool.map(terms, (command, ordered_differently)))
    assert sorted(reply[0] for reply in responses) == [200, 201]
    created = responses[0][2]
    assert responses[1][2] == created
    assert created["status"] == "draft"
    assert (created["proposal_id"], created["plan_id"], created["revision"], created["currency"], created["total"]) == (
        proposal["proposal_id"], proposal["plan_id"], 1, "EUR", "1882.40")
    assert [line["sku"] for line in created["lines"]] == [line["sku"] for line in proposal["lines"]]
    for saved, original in zip(created["lines"], proposal["lines"], strict=True):
        assert (saved["sku"], saved["revision"], saved["pieces"], saved["unit"]) == (
            original["sku"], original["revision"], original["pieces"], original["unit"])
        assert saved["unit_price"] == PRICES[original["sku"]]
        assert saved["line_total"] == f"{Decimal(saved['unit_price']) * int(saved['pieces']):.2f}"
        assert saved["supplier"] == {key: supplier[key] for key in ("id", "code", "name")}

    approval_command = None
    if os.getenv("ERP4US_BUDGET_APPROVAL_E2E") == "1":
        from procurement_approval import exercise as exercise_approval

        approval_command = exercise_approval(proposal, created)

    stale = {**command, "command_id": str(uuid4())}
    assert terms(stale)[0] == 409
    changed = deepcopy(command)
    changed["lines"][0]["unit_price"] = "50.00"
    assert terms(changed)[0] == 409
    new = {**changed, "command_id": str(uuid4()), "expected_revision": 1}
    competing = deepcopy(new)
    competing["command_id"] = str(uuid4())
    competing["lines"][0]["unit_price"] = "51.00"
    with ThreadPoolExecutor(max_workers=2) as pool:
        responses = list(pool.map(terms, (new, competing)))
    assert sorted(reply[0] for reply in responses) == [201, 409]
    latest = next(reply[2] for reply in responses if reply[0] == 201)
    assert latest["revision"] == 2
    manage("restart", "procurement")
    wait_until_ready(PROCUREMENT)
    assert terms(bearer=observer)[2] == latest
    assert terms(scope=(OTHER_TENANT, EXTERNAL_ORGANIZATION))[0] == 404
    if approval_command is not None:
        from procurement_approval import check_superseded

        check_superseded(proposal, latest, approval_command)

    try:
        manage("stop", "master-data")
        assert terms(command)[2] == created
        assert terms()[2] == latest
        new_intent = {**new, "command_id": str(uuid4()), "expected_revision": 2}
        assert terms(new_intent)[0] == 503
        assert terms()[2] == latest
    finally:
        manage("start", "master-data")
    wait_until_ready()
    status, _, untouched = request(path.removesuffix("/terms"), origin=PROCUREMENT)
    assert status == 200 and untouched == proposal and untouched["status"] == "draft"
    if approval_command is not None:
        from procurement_approval import check_revision_race

        check_revision_race(proposal, latest)
    print("Draft terms exact cents, owner checks, revision conflicts and outage-safe replay passed")
    if os.getenv("ERP4US_PURCHASE_ISSUANCE_E2E") == "1":
        from procurement_issuance import exercise as exercise_issuance

        assert approval_command is not None, "Issuance integration proof requires the budget-stage fixture"
        exercise_issuance(proposal, created, command)
