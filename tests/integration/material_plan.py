"""Exercise scoped, atomic BOM netting of the real NovaMotion order."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import subprocess

from delivery import eventually
from identity_fixture import EXTERNAL_ORGANIZATION, ORGANIZATION, OTHER_TENANT, TENANT, token
from sales_inventory import (COMPOSE, INVENTORY, WAREHOUSE, committed_reservation, confirmed, manage)
from smoke import mongo_eval, request, wait_until_ready


BUY = {"NM-ENDPLATE": 12, "NM-MOTOR": 6, "NM-SPINDLE": 10, "NM-RAIL": 8, "NM-SENSOR": 10}
MAKE = {"NM-1000": 10, "NM-1100": 10, "NM-1200": 10}


def main():
    if os.getenv("COMPOSE_PROJECT_NAME") in (None, "", "erp4us", "erp4us-preview"):
        raise RuntimeError("Run netting tests in a fresh disposable warehouse")
    wait_until_ready(INVENTORY)
    status, _, partners = request("/v1/partners?role=customer&after=BZ")
    assert status == 200
    atlas = next(partner["id"] for partner in partners["items"] if partner["code"] == "CUS-ATLAS")
    order = confirmed(atlas, "NM-1000")
    reservation = eventually(lambda: committed_reservation(order), "open finished-goods demand")
    assert reservation["outstanding"] == "10"
    path = f"/v1/inventory/reservations/{order['id']}/{order['line']['id']}/material-plan"

    def plan(*, create=False, **kwargs):
        return request(path, b"" if create else None, origin=INVENTORY, **kwargs)

    assert plan()[0] == 404
    assert plan(create=True, bearer=None)[0] == 401
    assert plan(create=True, bearer=token(subject="synthetic-planning-reader"))[0] == 403
    assert plan(create=True, bearer=token(subject="synthetic-planning-no-bom"))[0] == 403
    assert request(path, b'{"sku":"NM-2000","pieces":"1"}', origin=INVENTORY)[0] == 400

    manage("stop", "manufacturing")
    try:
        assert plan(create=True)[0] == 503
        assert plan()[0] == 404
    finally:
        manage("start", "manufacturing")
    manufacturing = f"http://127.0.0.1:{os.getenv('ERP4US_MANUFACTURING_PORT', '8090')}"
    wait_until_ready(manufacturing)
    with ThreadPoolExecutor(max_workers=2) as pool:
        replies = list(pool.map(lambda _: plan(create=True), range(2)))
    assert sorted(reply[0] for reply in replies) == [200, 201]
    stored = replies[0][2]
    assert replies[1][2] == stored
    assert (stored["order_id"], stored["line_id"], stored["source_event_id"]) == (
        order["id"], order["line"]["id"], order["confirmation_event_id"])
    assert (stored["sku"], stored["product_revision"], stored["warehouse_id"], stored["pieces"], stored["unit"]) == (
        "NM-1000", "A", WAREHOUSE, "10", "EA")
    assert {r["sku"]: int(r["uncovered"]) for r in stored["production"]} == MAKE
    assert {r["sku"]: int(r["uncovered"]) for r in stored["materials"] if r["uncovered"] != "0"} == BUY
    for row in stored["production"] + stored["materials"]:
        assert int(row["requested"]) == int(row["reserved"]) + int(row["uncovered"])
    assert {r["sku"]: int(r["reserved"]) for r in stored["materials"]} == {
        "NM-CABLE": 10, "NM-COUPLING": 10, "NM-ENDPLATE": 8, "NM-MOTOR": 4, "NM-PACK": 10,
        "NM-PROFILE": 20, "NM-RAIL": 12, "NM-SCREW": 120, "NM-SENSOR": 10, "NM-SPINDLE": 0,
    }
    assert plan(bearer=token(subject="synthetic-planning-reader"))[2] == stored
    assert plan(bearer=token(subject="synthetic-sales-observer"))[0] == 403
    assert plan(scope=(OTHER_TENANT, EXTERNAL_ORGANIZATION))[0] == 404
    # The trusted legacy fixture must not cancel a live line that already holds
    # components; cancellation needs a future operation that releases the plan.
    cancel = subprocess.run([
        *COMPOSE, "exec", "-T", "-e", f"INVENTORY_TENANT_ID={TENANT}",
        "-e", f"INVENTORY_ORGANIZATION_ID={ORGANIZATION}", "inventory", "/usr/local/bin/inventory",
    ], input=json.dumps({"contract": "cancel-stock-line.v1", "orderId": order["id"],
                        "lineId": order["line"]["id"]}), text=True, capture_output=True, timeout=15)
    assert cancel.returncode == 1 and cancel.stderr.strip() == "cancellation not acknowledged"
    assert committed_reservation(order)["outstanding"] == "10"

    manage("stop", "manufacturing")
    try:
        manage("restart", "inventory")
        wait_until_ready(INVENTORY)
        assert plan()[2] == plan(create=True)[2] == stored
    finally:
        manage("start", "manufacturing")
    wait_until_ready(manufacturing)
    other_order = confirmed(atlas, "NM-1000")
    eventually(lambda: committed_reservation(other_order), "second order after component allocation")
    other_route = f"/v1/inventory/reservations/{other_order['id']}/{other_order['line']['id']}/material-plan"
    status, _, other = request(other_route, b"", origin=INVENTORY)
    assert status == 201 and other["id"] != stored["id"]
    assert all(row["reserved"] == "0" and row["requested"] == row["uncovered"] for row in other["materials"])
    assert mongo_eval(COMPOSE, f"""print(db.getSiblingDB('inventory').balances.countDocuments({{
        "_id.tenantId": "{TENANT}", "_id.organizationId": "{ORGANIZATION}", available: {{$lt: 0}}
    }}))""") == 0
    print("NovaMotion exact net material shortages, atomic replay, restart and denied scope passed")
    if os.getenv("ERP4US_PROCUREMENT_E2E") == "1":
        from procurement_handoff import exercise
        exercise(stored, BUY)


if __name__ == "__main__":
    main()
