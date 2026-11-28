"""Verify budget-stage decisions against exact saved NovaMotion terms."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
from uuid import uuid4

from identity_fixture import ORGANIZATION, OTHER_ORGANIZATION, TENANT, token
from procurement_handoff import PROCUREMENT, manage
from procurement_issuance import check_unissued
from smoke import request, wait_until_ready


def exercise(proposal, terms):
    base = f"/v1/procurement/proposals/by-plan/{proposal['plan_id']}"
    path = f"{base}/approval"
    check_issuance = os.getenv("ERP4US_PURCHASE_ISSUANCE_E2E") == "1"
    department = token(subject="synthetic-department-approver")
    budget = token(subject="synthetic-budget-approver")

    def approval(command=None, **kwargs):
        data = json.dumps(command).encode() if command is not None else None
        return request(path, data, origin=PROCUREMENT, timeout=11, **kwargs)

    status, headers, pending = approval()
    assert status == 200 and headers["Cache-Control"] == "no-store"
    assert (pending["proposal_id"], pending["plan_id"], pending["terms_revision"], pending["currency"],
            pending["total"], pending["state"]) == (
        proposal["proposal_id"], proposal["plan_id"], terms["revision"], "EUR", "1882.40", "pending")
    assert pending["stages"] == [{"key": stage, "state": "pending"} for stage in ("department", "budget")]
    if check_issuance:
        check_unissued(base, pending)
    assert approval(bearer=None)[0] == 401
    assert approval(bearer=budget, scope=(TENANT, OTHER_ORGANIZATION))[0] == 404

    first = {
        "command_id": str(uuid4()), "terms_revision": terms["revision"],
        "policy_id": pending["policy_id"], "policy_revision": pending["policy_revision"], "stage_key": "department",
    }
    second = {**first, "command_id": str(uuid4()), "stage_key": "budget"}
    assert approval(first)[0] == 403
    assert approval(first, bearer=token(subject="synthetic-dual-role-buyer"))[0] == 403
    assert approval(first, bearer=budget)[0] == 403
    assert approval(second, bearer=budget)[0] == 409                  # Not yet the next stage.
    assert approval({**first, "total": "0.00"}, bearer=department)[0] == 400
    assert approval({**first, "policy_revision": first["policy_revision"] + 1}, bearer=department)[0] == 409
    assert approval()[2] == pending

    with ThreadPoolExecutor(max_workers=2) as pool:
        replies = list(pool.map(lambda _: approval(first, bearer=department), range(2)))
    assert sorted(reply[0] for reply in replies) == [200, 201], [reply[0] for reply in replies]
    one_stage = replies[0][2]
    assert replies[1][2] == one_stage and one_stage["state"] == "pending"
    assert one_stage["stages"] == [{"key": "department", "state": "approved"}, {"key": "budget", "state": "pending"}]
    if check_issuance:
        check_unissued(base, one_stage)
    assert approval(second, bearer=department)[0] == 403            # Has grant, but not the distinct actor.
    assert approval({**second, "command_id": first["command_id"]}, bearer=department)[0] == 409

    status, _, approved = approval(second, bearer=budget)
    assert status == 201 and approved["state"] == "approved"
    assert approved["stages"] == [{"key": stage, "state": "approved"} for stage in ("department", "budget")]
    assert set(approved) == set(pending)
    if check_issuance:
        check_unissued(base, approved)
    manage("restart", "procurement")
    wait_until_ready(PROCUREMENT)
    assert approval()[2] == approved and approval(first, bearer=department)[2] == one_stage
    status, _, unchanged = request(base, origin=PROCUREMENT, scope=(TENANT, ORGANIZATION))
    assert status == 200 and unchanged == proposal and unchanged["status"] == "draft"
    print("Both configured budget stages required; exact revision, role separation, restart and replay passed")
    return first


def check_superseded(proposal, revised_terms, first_command):
    path = f"/v1/procurement/proposals/by-plan/{proposal['plan_id']}/approval"
    status, _, now = request(path, origin=PROCUREMENT, timeout=11)
    assert status == 200 and now["state"] == "pending" and now["terms_revision"] == revised_terms["revision"]
    assert all(stage["state"] == "pending" for stage in now["stages"])
    new_command = {**first_command, "command_id": str(uuid4())}
    status, _, _ = request(path, json.dumps(new_command).encode(), origin=PROCUREMENT, timeout=11,
                          bearer=token(subject="synthetic-department-approver"))
    assert status == 409


def check_revision_race(proposal, terms):
    base = f"/v1/procurement/proposals/by-plan/{proposal['plan_id']}"
    status, _, before = request(f"{base}/approval", origin=PROCUREMENT, timeout=11)
    assert status == 200 and before["state"] == "pending"
    assert before["terms_revision"] == terms["revision"]
    assert all(stage["state"] == "pending" for stage in before["stages"])
    department = token(subject="synthetic-department-approver")
    approve = json.dumps({
        "command_id": str(uuid4()), "terms_revision": terms["revision"], "policy_id": before["policy_id"],
        "policy_revision": before["policy_revision"], "stage_key": "department",
    }).encode()
    lines = [{"sku": row["sku"], "revision": row["revision"], "supplier_id": row["supplier"]["id"],
              "unit_price": row["unit_price"]} for row in terms["lines"]]
    lines[0]["unit_price"] = "60.00" if lines[0]["unit_price"] != "60.00" else "61.00"
    edit = json.dumps({"command_id": str(uuid4()), "expected_revision": terms["revision"], "lines": lines}).encode()
    with ThreadPoolExecutor(max_workers=2) as pool:
        approved = pool.submit(request, f"{base}/approval", approve, bearer=department, origin=PROCUREMENT, timeout=11)
        saved = pool.submit(request, f"{base}/terms", edit, origin=PROCUREMENT, timeout=11)
        approval_status, _, stage = approved.result()
        save_status, _, latest = saved.result()
    assert save_status == 201 and latest["revision"] == terms["revision"] + 1
    assert approval_status in (201, 409), approval_status
    if approval_status == 201:
        # The stage may have won the race, but can only describe the old terms.
        assert stage["terms_revision"] == terms["revision"]
        assert stage["stages"][0]["state"] == "approved"
        assert request(f"{base}/approval", approve, bearer=department, origin=PROCUREMENT, timeout=11)[2] == stage
    status, _, now = request(f"{base}/approval", origin=PROCUREMENT, timeout=11)
    assert status == 200 and now["terms_revision"] == latest["revision"]
    assert now["state"] == "pending" and all(row["state"] == "pending" for row in now["stages"])
    print("Concurrent terms change and stage approval cannot carry a decision to revised terms")
