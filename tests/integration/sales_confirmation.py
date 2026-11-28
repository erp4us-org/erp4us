"""Prove a confirmation survives an outage and reaches JetStream after recovery."""
import base64
from concurrent.futures import ThreadPoolExecutor
import json
import subprocess
from time import sleep
from uuid import uuid4

from delivery import broker_probe, eventually
from identity_fixture import EXTERNAL_ORGANIZATION, ORGANIZATION, OTHER_TENANT, TENANT, token
from sales_drafts import ORIGIN, ORDERS, post
from smoke import env, mongo_eval, request, wait_until_ready


COMPOSE = ["docker", "compose", "-f", "compose.yaml", "-f", "compose.delivery.yaml",
           "-f", "compose.manufacturing.yaml", "-f", "compose.sales.yaml",
           "-f", "compose.sales-events.yaml"]
SUBJECT = f"v1.business.{TENANT}.sales.event.order.confirmed.v1"
STREAM = "ERP4US_BUSINESS_EVENTS_V1"


def manage(*args):
    subprocess.run([*COMPOSE, *args], check=True, timeout=45)


def js_query(operation, payload):
    inbox = f"_INBOX.sales-confirmation-test.{uuid4().hex}"
    body = json.dumps(payload, separators=(",", ":"), ensure_ascii=True)
    lines = broker_probe(COMPOSE, "provisioner", env["DEV_NATS_ADMIN_PASSWORD"], [
        f"SUB {inbox} 1", f"PUB $JS.API.STREAM.{operation}.{STREAM} {inbox} {len(body)}", body,
    ]).splitlines()
    frame = next(index for index, line in enumerate(lines) if line.startswith(f"MSG {inbox} "))
    return json.loads(lines[frame + 1])


def subject_count():
    state = js_query("INFO", {"subjects_filter": SUBJECT})["state"]
    return (state.get("subjects") or {}).get(SUBJECT, 0)


def mongo_confirmation(order_id):
    return mongo_eval(COMPOSE, "print(JSON.stringify(db.getSiblingDB('sales').orders.findOne("
                      f"{{'tenant_id': '{TENANT}', 'organization_id': '{ORGANIZATION}',"
                      f"'draft.id': {json.dumps(order_id)}}}, {{_id: 0, confirmation: 1}})"
                      ".confirmation || null))")


def main():
    wait_until_ready(ORIGIN)
    status, _, customers = request("/v1/partners?role=customer&after=BZ")
    assert status == 200
    atlas = next(item for item in customers["items"] if item["code"] == "CUS-ATLAS")
    draft_input = {"id": str(uuid4()), "customer_id": atlas["id"],
                   "sku": "NM-1000", "product_revision": "A", "pieces": "10"}
    status, _, draft = post(draft_input)
    assert status == 201 and draft["total"] == "24900.00"
    path = f"{ORDERS}/{draft['id']}"
    command = {"command_id": str(uuid4()), "expected_revision": 1}

    def confirm(payload, **kwargs):
        return request(path + "/confirmation", json.dumps(payload).encode(), origin=ORIGIN, **kwargs)

    assert confirm(command, bearer=None)[0] == 401
    observer = token(subject="synthetic-sales-observer")
    assert request(path, origin=ORIGIN, bearer=observer)[0] == 200
    assert confirm(command, bearer=observer)[0] == 403
    assert confirm(command, scope=(OTHER_TENANT, EXTERNAL_ORGANIZATION))[0] == 403
    assert confirm({**command, "expected_revision": 2})[0] == 409
    assert mongo_confirmation(draft["id"]) is None

    baseline = subject_count()
    manage("stop", "nats")
    try:
        with ThreadPoolExecutor(max_workers=2) as pool:
            first, second = pool.map(confirm, [command, command])
        assert first[0] == second[0] == 200
        confirmed = first[2]
        assert confirmed == second[2]
        assert confirmed["status"] == "confirmed" and confirmed["revision"] == 2
        assert confirmed["customer"] == draft["customer"] and confirmed["line"] == draft["line"]
        assert confirmed["total"] == "24900.00"
        event_id = confirmed["confirmation_event_id"]
        pending = mongo_confirmation(draft["id"])
        assert pending["pending"] is True and pending["message_id"] == event_id
        assert "published_at" not in pending
        assert confirm({**command, "command_id": str(uuid4())})[0] == 409
        assert confirm({**command, "expected_revision": 2})[0] == 409
        assert post(draft_input)[2] == draft  # Original create response stays a draft snapshot.
        manage("restart", "sales")
        wait_until_ready(ORIGIN)
        assert request(path, origin=ORIGIN)[2] == confirmed
        assert confirm(command)[2] == confirmed
    finally:
        manage("start", "nats")
    manage("restart", "sales-relay")
    eventually(lambda: mongo_confirmation(draft["id"]).get("published_at"), "Sales outbox acknowledgement")
    message = js_query("MSG.GET", {"last_by_subj": SUBJECT})["message"]
    event = json.loads(base64.b64decode(message["data"]))
    headers = base64.b64decode(message["hdrs"]).decode()
    assert event["messageId"] == event["correlationId"] == event_id
    assert event["tenantId"] == TENANT and event["organizationId"] == ORGANIZATION
    assert event["payload"] == {
        "orderId": draft["id"], "revision": 2, "lineId": draft["line"]["id"], "sku": "NM-1000",
        "productRevision": "A", "unit": "EA", "pieces": "10",
    }
    assert event["schemaVersion"] == event["protocolVersion"] == "1.0.0"
    assert f"Nats-Msg-Id: {event_id}".lower() in headers.lower()
    assert f"Erp4us-Message-Id: {event_id}".lower() in headers.lower()
    assert subject_count() == baseline + 1
    assert confirm(command)[2] == confirmed
    manage("restart", "sales-relay")
    assert subject_count() == baseline + 1

    # Fixture-only fault injection: model a lost database update after the
    # broker acknowledgement, then wait beyond this stream's dedup window.
    # The relay must replay the same wire identity; transport is at least once.
    manage("stop", "sales-relay")
    try:
        changed = mongo_eval(COMPOSE, ("print(db.getSiblingDB('sales').orders.updateOne("
            f"{{'tenant_id': '{TENANT}', 'organization_id': '{ORGANIZATION}',"
            f"'draft.id': {json.dumps(draft['id'])}, 'confirmation.message_id': {json.dumps(event_id)}}},"
            "{$set: {'confirmation.pending': true}, $unset: {'confirmation.published_at': ''}}).modifiedCount)"))
        assert changed == 1
        sleep(2.1)
    finally:
        manage("start", "sales-relay")
    eventually(lambda: mongo_confirmation(draft["id"]).get("published_at"), "acknowledgement after replay")
    replay = js_query("MSG.GET", {"last_by_subj": SUBJECT})["message"]
    assert replay["seq"] != message["seq"]
    assert replay["data"] == message["data"]
    # Header order is not part of the NATS wire identity; retain multiplicity.
    assert sorted(base64.b64decode(replay["hdrs"]).decode().splitlines()) == sorted(headers.splitlines())
    assert subject_count() == baseline + 2
    assert request(path, origin=ORIGIN)[2] == confirmed
    print("Atomic Sales confirmation, NATS outage/recovery and stable at-least-once replay passed")


if __name__ == "__main__":
    main()
