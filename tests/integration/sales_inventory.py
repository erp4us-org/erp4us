"""Run the real Sales → JetStream → Inventory path on a disposable Compose stack."""
import base64
from concurrent.futures import ThreadPoolExecutor
import json
import os
import subprocess
from time import sleep
from uuid import uuid4

from delivery import broker_probe, eventually
from identity_fixture import EXTERNAL_ORGANIZATION, ORGANIZATION, OTHER_TENANT, TENANT, token
from sales_confirmation import SUBJECT, STREAM, js_query
from sales_drafts import ORDERS, ORIGIN as SALES, post
from smoke import env, mongo_eval, request, wait_until_ready


INVENTORY = f"http://127.0.0.1:{os.getenv('ERP4US_INVENTORY_PORT', '8093')}"
COMPOSE = ["docker", "compose", "-f", "compose.yaml", "-f", "compose.delivery.yaml",
           "-f", "compose.manufacturing.yaml", "-f", "compose.sales.yaml", "-f", "compose.sales-events.yaml",
           "-f", "compose.inventory.yaml"]
WAREHOUSE = "49e7372b-0b8d-4363-b610-99ce38348825"


def manage(*args):
    subprocess.run([*COMPOSE, *args], check=True, timeout=45)


def lookup(order, **kwargs):
    return request(f"/v1/inventory/reservations/{order['id']}/{order['line']['id']}",
                   origin=INVENTORY, **kwargs)


def committed_reservation(order):
    status, headers, body = lookup(order)
    assert status in (200, 404), status
    if status == 200:
        assert headers["Cache-Control"] == "no-store"
        assert (body["order_id"], body["line_id"], body["source_event_id"]) == (
            order["id"], order["line"]["id"], order["confirmation_event_id"])
        assert body["warehouse_id"] == WAREHOUSE and body["unit"] == "EA"
        return body
    return None


def confirmed(customer_id, sku):
    status, _, body = post({"id": str(uuid4()), "customer_id": customer_id, "sku": sku,
                            "product_revision": "A", "pieces": "10"})
    assert status == 201
    command = {"command_id": str(uuid4()), "expected_revision": 1}
    status, _, order = request(f"{ORDERS}/{body['id']}/confirmation", json.dumps(command).encode(), origin=SALES)
    assert status == 200
    return order


def by_event(order, collection):
    key = {"tenantId": TENANT, "organizationId": ORGANIZATION, "messageId": order["confirmation_event_id"]}
    return mongo_eval(COMPOSE, "print(db.getSiblingDB('inventory')."
                      f"{collection}.countDocuments({{_id: {json.dumps(key)}}}))")


def warehouse_quantity():
    scope = {"_id.tenantId": TENANT, "_id.organizationId": ORGANIZATION,
             "_id.warehouseId": WAREHOUSE, "_id.catalogSku": "NM-2000", "_id.productRevision": "A"}
    return mongo_eval(COMPOSE, "print(db.getSiblingDB('inventory').balances."
                      f"findOne({json.dumps(scope)}).available)")


def broker_request(subject, data):
    inbox = f"_INBOX.inventory-link-test.{uuid4().hex}"
    body = json.dumps(data, separators=(",", ":"), ensure_ascii=True)
    lines = broker_probe(COMPOSE, "provisioner", env["DEV_NATS_ADMIN_PASSWORD"], [
        f"SUB {inbox} 1", f"PUB {subject} {inbox} {len(body)}", body,
    ]).splitlines()
    frame = next(i for i, line in enumerate(lines) if line.startswith(f"MSG {inbox} "))
    return json.loads(lines[frame + 1])


def check_wrong_organization():
    # Fault injection only. The broker ACL is tenant-level; the service must
    # also reject a correctly formed message for the unconfigured organization.
    retained = js_query("MSG.GET", {"last_by_subj": SUBJECT})["message"]
    event = json.loads(base64.b64decode(retained["data"]))
    event["messageId"] = event["correlationId"] = str(uuid4())
    event["organizationId"] = EXTERNAL_ORGANIZATION
    event["payload"]["orderId"] = str(uuid4())
    encoded = json.dumps(event, separators=(",", ":"), ensure_ascii=True)
    fields = {"Protocol-Version": "protocolVersion", "Schema-Version": "schemaVersion",
              "Service-Version": "serviceVersion", "Source": "sourceService",
              "Message-Id": "messageId", "Correlation-Id": "correlationId",
              "Tenant-Id": "tenantId", "Organization-Id": "organizationId"}
    headers = "NATS/1.0\r\nContent-Type: application/json\r\n" + "".join(
        f"Erp4us-{header}: {event[field]}\r\n" for header, field in fields.items()
    ) + f"Nats-Msg-Id: {event['messageId']}\r\n\r\n"
    inbox = f"_INBOX.inventory-scope-test.{uuid4().hex}"
    lines = broker_probe(COMPOSE, "provisioner", env["DEV_NATS_ADMIN_PASSWORD"], [
        f"SUB {inbox} 1", f"HPUB {SUBJECT} {inbox} {len(headers)} {len(headers) + len(encoded)}",
        headers + encoded,
    ]).splitlines()
    frame = next(i for i, line in enumerate(lines) if line.startswith(f"MSG {inbox} "))
    sequence = json.loads(lines[frame + 1])["seq"]
    eventually(lambda: mongo_eval(COMPOSE, ("print(db.getSiblingDB('inventory').sales_quarantine.countDocuments(" +
        json.dumps({"_id": f"{STREAM}:{sequence}", "reason": "invalid-or-forbidden-envelope"}) + "))")),
        "organization denial quarantine")
    foreign_key = {"tenantId": TENANT, "organizationId": EXTERNAL_ORGANIZATION,
                   "orderId": event["payload"]["orderId"], "lineId": event["payload"]["lineId"]}
    assert mongo_eval(COMPOSE, "print(db.getSiblingDB('inventory').reservations.countDocuments("
                      f"{{_id: {json.dumps(foreign_key)}}}))") == 0


def main():
    if os.getenv("COMPOSE_PROJECT_NAME") in (None, "", "erp4us", "erp4us-preview"):
        raise RuntimeError("Run Inventory integration against a fresh, explicitly named test stack")
    wait_until_ready(INVENTORY)
    status, _, partners = request("/v1/partners?role=customer&after=BZ")
    assert status == 200
    atlas = next(row["id"] for row in partners["items"] if row["code"] == "CUS-ATLAS")
    assert warehouse_quantity() == 6, "Use a fresh warehouse seed; it intentionally never restocks"
    manage("stop", "inventory-worker")
    try:
        with ThreadPoolExecutor(max_workers=2) as pool:
            competing = list(pool.map(lambda _: confirmed(atlas, "NM-2000"), range(2)))
        scenario = confirmed(atlas, "NM-1000")
        assert lookup(scenario)[0] == 404
    finally:
        manage("start", "inventory-worker")
    outcomes = [eventually(lambda: committed_reservation(order), "warehouse allocation") for order in competing]
    assert sorted((int(row["reserved"]), int(row["outstanding"])) for row in outcomes) == [(0, 10), (6, 4)]
    snapshot = eventually(lambda: committed_reservation(scenario), "NovaMotion finished-goods demand")
    assert (snapshot["requested"], snapshot["reserved"], snapshot["outstanding"]) == ("10", "0", "10")
    assert lookup(scenario, bearer=None)[0] == 401
    assert lookup(scenario, bearer=token(subject="synthetic-sales-observer"))[0] == 403
    assert lookup(scenario, scope=(OTHER_TENANT, EXTERNAL_ORGANIZATION))[0] == 404
    manage("run", "--rm", "-T", "inventory-seed")
    assert warehouse_quantity() == 0

    # Model a Sales relay crash after broker ack but before its database update.
    # A later replay should produce another broker sequence but no new allocation.
    winner = next(order for order, row in zip(competing, outcomes, strict=True) if row["reserved"] == "6")
    before = lookup(winner)[2]
    selector = {"tenant_id": TENANT, "organization_id": ORGANIZATION, "draft.id": winner["id"],
                "confirmation.message_id": winner["confirmation_event_id"], "confirmation.pending": False}
    manage("stop", "sales-relay")
    try:
        assert mongo_eval(COMPOSE, "print(db.getSiblingDB('sales').orders.updateOne("
                          f"{json.dumps(selector)}, {{$set: {{'confirmation.pending': true}},"
                          "$unset: {'confirmation.published_at': ''}}).modifiedCount)") == 1
        sleep(2.1)
    finally:
        manage("start", "sales-relay")

    def replay_sequence():
        response = js_query("MSG.GET", {"last_by_subj": SUBJECT})["message"]
        envelope = json.loads(base64.b64decode(response["data"]))
        return response["seq"] if envelope["messageId"] == winner["confirmation_event_id"] else None

    replayed = eventually(replay_sequence, "broker replay with stable message ID")
    eventually(lambda: broker_request(f"$JS.API.CONSUMER.INFO.{STREAM}.inventory-sales-v1", {})
               ["ack_floor"]["stream_seq"] >= replayed, "Inventory acknowledgement after replay")
    assert by_event(winner, "sales_inbox") == 1 and lookup(winner)[2] == before
    assert warehouse_quantity() == 0
    check_wrong_organization()
    print("Sales-to-Inventory partial allocation, worker recovery, scope quarantine and replay passed")


if __name__ == "__main__":
    main()
