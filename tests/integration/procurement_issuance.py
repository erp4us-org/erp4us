"""Prove scoped, explicit local purchase issuance after complete budget approval."""
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
from decimal import Decimal
import json
from uuid import UUID, uuid4

from identity_fixture import EXTERNAL_ORGANIZATION, OTHER_ORGANIZATION, OTHER_TENANT, TENANT, token
from procurement_handoff import PROCUREMENT, manage
from sales_inventory import INVENTORY
from smoke import request, wait_until_ready


def command_for(approval):
    return {**{key: approval[key] for key in ("terms_revision", "policy_id", "policy_revision")},
            "command_id": str(uuid4())}


def purchase(base, command=None, **kwargs):
    return call(base + "/purchase-orders", command, **kwargs)


def call(path, command=None, **kwargs):
    data = None if command is None else json.dumps(command).encode()
    return request(path, data, origin=PROCUREMENT, timeout=11, **kwargs)


def check_unissued(base, approval):
    issuer = token(subject="synthetic-purchase-issuer")
    assert purchase(base, bearer=issuer)[0] == 404
    if approval["state"] == "pending":
        assert purchase(base, command_for(approval), bearer=issuer)[0] == 409
        assert purchase(base, bearer=issuer)[0] == 404


def exercise(proposal, original_terms, original_command):
    base = f"/v1/procurement/proposals/by-plan/{proposal['plan_id']}"
    inventory = f"/v1/inventory/reservations/{proposal['order_id']}/{proposal['line_id']}"
    plan_status, _, plan_before = request(inventory + "/material-plan", origin=INVENTORY)
    reservation_status, _, reservation_before = request(inventory, origin=INVENTORY)
    assert plan_status == reservation_status == 200

    status, _, terms = call(base + "/terms")
    assert status == 200 and terms["revision"] > original_terms["revision"]
    status, _, pending = call(base + "/approval")
    assert status == 200 and pending["state"] == "pending"
    issuer = token(subject="synthetic-purchase-issuer")
    approver = token(subject="synthetic-department-approver")
    reviewer = token(subject="synthetic-budget-approver")
    dual_role = token(subject="synthetic-dual-role-buyer")
    issue = command_for(pending)
    assert purchase(base, issue, bearer=issuer)[0] == 409
    for stage, actor in (("department", approver), ("budget", reviewer)):
        review = {**command_for(pending), "stage_key": stage}
        status, _, approved = call(base + "/approval", review, bearer=actor)
        assert status == 201
    assert approved["state"] == "approved"
    # Approving both stages does not create an order, even after an earlier
    # issuance attempt was denied. The preparer is denied despite both grants.
    assert purchase(base)[0] == 404
    assert purchase(base, issue)[0] == 403
    assert purchase(base, issue, bearer=dual_role)[0] == 403
    assert purchase(base, issue, bearer=reviewer)[0] == 403
    assert purchase(base, issue, bearer=None)[0] == 401
    assert purchase(base, bearer=reviewer)[0] == 403
    assert purchase(base, {**issue, "total": "0.00"}, bearer=issuer)[0] == 400
    assert purchase(base, {**issue, "terms_revision": original_terms["revision"]}, bearer=issuer)[0] == 409
    assert purchase(base, {**issue, "policy_revision": issue["policy_revision"] + 1}, bearer=issuer)[0] == 409
    assert purchase(base, bearer=issuer)[0] == 404

    with ThreadPoolExecutor(max_workers=2) as pool:
        responses = list(pool.map(lambda _: purchase(base, issue, bearer=issuer), range(2)))
    assert sorted(reply[0] for reply in responses) == [200, 201]
    batch = responses[0][2]
    assert responses[1][2] == batch
    assert all(reply[1]["Cache-Control"] == "no-store" for reply in responses)
    refs = ("plan_id", "proposal_id", "order_id", "line_id")
    assert all(batch[key] == proposal[key] for key in refs)
    assert set(batch) == {*refs, "batch_id", "terms_revision", "policy_id", "policy_revision",
                          "state", "currency", "total", "issued_at", "purchase_orders"}
    assert (batch["state"], batch["currency"], batch["total"]) == ("issued", "EUR", terms["total"])
    assert all(batch[key] == issue[key] for key in ("terms_revision", "policy_id", "policy_revision"))
    assert datetime.fromisoformat(batch["issued_at"]).utcoffset().total_seconds() == 0
    assert len(batch["purchase_orders"]) == 1
    order = batch["purchase_orders"][0]
    assert order["total"] == batch["total"]
    assert len(order["lines"]) == len(terms["lines"])
    ids = {batch["batch_id"], order["purchase_order_id"]}
    by_item = {(line["sku"], line["revision"]): line for line in terms["lines"]}
    for line in order["lines"]:
        saved = by_item.pop((line["sku"], line["revision"]))
        assert order["supplier"] == saved["supplier"]
        assert all(line[key] == saved[key] for key in ("pieces", "unit", "unit_price", "line_total"))
        ids.add(line["purchase_line_id"])
    assert not by_item and len(ids) == 2 + len(order["lines"])
    assert all(str(UUID(value)) == value and UUID(value).int for value in ids)
    assert sum(Decimal(line["line_total"]) for line in order["lines"]) == Decimal(batch["total"])

    assert purchase(base, {**issue, "command_id": str(uuid4())}, bearer=issuer)[0] == 409
    # A distinct actor cannot use the original idempotency receipt.
    assert purchase(base, issue, bearer=dual_role)[0] == 409
    for scope in ((TENANT, OTHER_ORGANIZATION), (OTHER_TENANT, EXTERNAL_ORGANIZATION)):
        assert purchase(base, scope=scope, bearer=issuer)[0] == 404
        assert purchase(base, issue, scope=scope, bearer=issuer)[0] == 404
    edit = {"command_id": str(uuid4()), "expected_revision": terms["revision"], "lines": [
        {**{key: line[key] for key in ("sku", "revision", "unit_price")}, "supplier_id": line["supplier"]["id"]}
        for line in terms["lines"]
    ]}
    edit["lines"][0]["unit_price"] = "99.00"
    assert call(base + "/terms", edit)[0] == 409
    assert call(base + "/terms")[2] == terms
    assert call(base + "/terms", original_command)[2] == original_terms
    manage("restart", "procurement")
    wait_until_ready(PROCUREMENT)
    assert purchase(base)[2] == batch
    status, _, replay = purchase(base, issue, bearer=issuer)
    assert status == 200 and replay == batch
    assert request(inventory, origin=INVENTORY)[2] == reservation_before
    assert request(inventory + "/material-plan", origin=INVENTORY)[2] == plan_before
    print("Explicit issuance: one exact local order, stable concurrent replay and restart, frozen terms, no stock effect")
