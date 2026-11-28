"""Exercise a plan-to-draft handoff with a broker outage and a stable replay."""
import base64
from concurrent.futures import ThreadPoolExecutor
from copy import deepcopy
import json
import os
import subprocess
from time import sleep
from uuid import uuid4

from delivery import broker_probe, eventually
from identity_fixture import EXTERNAL_ORGANIZATION, ORGANIZATION, OTHER_TENANT, TENANT, token
from sales_confirmation import js_query
from sales_inventory import COMPOSE as INVENTORY_COMPOSE, INVENTORY, broker_request
from smoke import env, mongo_eval, request, wait_until_ready


COMPOSE = [*INVENTORY_COMPOSE, "-f", "compose.procurement.yaml"]
PROCUREMENT = f"http://127.0.0.1:{os.getenv('ERP4US_PROCUREMENT_PORT', '8094')}"
SUBJECT = f"v1.business.{TENANT}.inventory.event.material-plan.netted.v1"
STREAM = "ERP4US_BUSINESS_EVENTS_V1"
DURABLE = "procurement-material-plans-v1"


def manage(*args):
    subprocess.run([*COMPOSE, *args], check=True, timeout=45)


def publish_fixture(event):
    # Operator fault injection on the isolated test broker, not a preview workflow.
    encoded = json.dumps(event, separators=(",", ":"), ensure_ascii=True)
    fields = {"Protocol-Version": "protocolVersion", "Schema-Version": "schemaVersion",
              "Service-Version": "serviceVersion", "Source": "sourceService", "Message-Id": "messageId",
              "Correlation-Id": "correlationId", "Causation-Id": "causationId",
              "Tenant-Id": "tenantId", "Organization-Id": "organizationId"}
    headers = "NATS/1.0\r\nContent-Type: application/json\r\n" + "".join(
        f"Erp4us-{header}: {event[field]}\r\n" for header, field in fields.items()
    ) + f"Nats-Msg-Id: {event['messageId']}\r\n\r\n"
    inbox = f"_INBOX.procurement-test.{uuid4().hex}"
    lines = broker_probe(COMPOSE, "provisioner", env["DEV_NATS_ADMIN_PASSWORD"], [
        f"SUB {inbox} 1", f"HPUB {SUBJECT} {inbox} {len(headers)} {len(headers) + len(encoded)}",
        headers + encoded,
    ]).splitlines()
    frame = next(i for i, line in enumerate(lines) if line.startswith(f"MSG {inbox} "))
    return json.loads(lines[frame + 1])["seq"]


def acked(sequence):
    return broker_request(f"$JS.API.CONSUMER.INFO.{STREAM}.{DURABLE}", {})["ack_floor"]["stream_seq"] >= sequence


def check_broker_permissions():
    foreign = SUBJECT.replace(TENANT, OTHER_TENANT)
    for user, password, denied_publish in (
        ("inventory_plan_relay", "DEV_NATS_INVENTORY_PLAN_PASSWORD", foreign),
        ("procurement_inventory", "DEV_NATS_PROCUREMENT_PASSWORD", SUBJECT),
    ):
        denied_admin = "$JS.API.STREAM.NAMES"
        denied_subscribe = f"v1.business.{TENANT}.>"
        result = broker_probe(COMPOSE, user, env[password], [
            f"PUB {denied_publish} 2", "{}", f"PUB {denied_admin} 2", "{}",
            f"SUB {denied_subscribe} 1",
        ])
        for denied in (denied_publish, denied_admin, denied_subscribe):
            assert any("Permissions Violation" in line and denied in line for line in result.splitlines()), denied


def check_business_replays(event):
    equivalent = deepcopy(event)
    equivalent["messageId"] = str(uuid4())
    equivalent["payload"]["buyRows"].reverse()
    sequence = publish_fixture(equivalent)
    eventually(lambda: acked(sequence), "equivalent plan acknowledged")
    key = {"tenantId": TENANT, "organizationId": ORGANIZATION, "messageId": equivalent["messageId"]}
    assert mongo_eval(COMPOSE, "print(db.getSiblingDB('procurement').material_plan_inbox.countDocuments("
                      f"{{_id: {json.dumps(key)}}}))") == 1

    conflicting = deepcopy(event)
    conflicting["messageId"] = str(uuid4())
    conflicting["payload"]["buyRows"][0]["pieces"] = "1"
    foreign = deepcopy(event)
    foreign["messageId"] = str(uuid4())
    foreign["organizationId"] = EXTERNAL_ORGANIZATION
    foreign["payload"]["planId"] = str(uuid4())
    for invalid, reason in ((conflicting, "conflicting-plan"), (foreign, "invalid-or-forbidden-envelope")):
        sequence = publish_fixture(invalid)
        selector = {"_id": f"{STREAM}:{sequence}", "reason": reason, "payload": {"$exists": False}}
        eventually(lambda: mongo_eval(COMPOSE, "print(db.getSiblingDB('procurement')."
                   f"material_plan_quarantine.countDocuments({json.dumps(selector)}))"), reason)
    assert mongo_eval(COMPOSE, "print(db.getSiblingDB('procurement').material_plan_drafts.countDocuments({}))") == 1


def wait_for_pipeline_backlog(cleared):
    from diagnostics import wait

    def expected(report):
        pipeline = report["jetstream"]["pipeline_consumers"]
        inventory, procurement = pipeline["inventory_sales"], pipeline["procurement_material_plans"]
        return inventory is not None and procurement is not None and (
            (procurement["pending"] == 0 and procurement["ack_pending"] == 0)
            if cleared else procurement["pending"] > 0)

    return wait(token(subject="synthetic-platform"), {"jetstream": "ready"}, expected)


def exercise(plan, expected):
    wait_until_ready(PROCUREMENT)
    check_broker_permissions()
    path = f"/v1/inventory/reservations/{plan['order_id']}/{plan['line_id']}/material-plan/procurement-handoff"
    draft_path = f"/v1/procurement/proposals/by-plan/{plan['id']}"

    def handoff(*, send=False, **kwargs):
        return request(path, b"" if send else None, origin=INVENTORY, **kwargs)

    def draft(**kwargs):
        return request(draft_path, origin=PROCUREMENT, **kwargs)

    def read_draft():
        status, headers, result = draft()
        assert status in (200, 404)
        if status == 200:
            assert headers["Cache-Control"] == "no-store"
            return result
        return None

    assert draft()[0] == handoff()[0] == 404
    assert handoff(send=True, bearer=None)[0] == 401
    reader = token(subject="synthetic-planning-reader")
    assert handoff(send=True, bearer=reader)[0] == 403
    assert request(path, b'{"buyRows":[]}', origin=INVENTORY)[0] == 400

    check_metrics = os.getenv("ERP4US_PIPELINE_DIAGNOSTICS_E2E") == "1"
    if check_metrics:
        manage("stop", "procurement-worker")
    manage("stop", "nats")
    try:
        with ThreadPoolExecutor(max_workers=2) as pool:
            replies = list(pool.map(lambda _: handoff(send=True), range(2)))
        assert sorted(reply[0] for reply in replies) == [200, 201], [reply[0] for reply in replies]
        sent = replies[0][2]
        assert sent == replies[1][2]
        assert sent["state"] == "pending" and sent["plan_id"] == plan["id"]
        assert draft()[0] == 404
        manage("restart", "inventory")
        wait_until_ready(INVENTORY)
        assert handoff()[2] == handoff(send=True)[2] == sent
    finally:
        manage("start", "nats")
    if check_metrics:
        try:
            manage("restart", "inventory-plan-relay")
            wait_for_pipeline_backlog(cleared=False)
        finally:
            manage("start", "procurement-worker")
    else:
        manage("restart", "inventory-plan-relay", "procurement-worker")
    proposal = eventually(read_draft, "Procurement draft after broker recovery")
    if check_metrics:
        wait_for_pipeline_backlog(cleared=True)
        print("Platform diagnostics observed Procurement backlog before consumption and zero after acknowledgement")
    assert (proposal["plan_id"], proposal["order_id"], proposal["line_id"], proposal["status"]) == (
        plan["id"], plan["order_id"], plan["line_id"], "draft")
    assert proposal["bom"] == {"sku": plan["sku"], "revision": plan["product_revision"]}
    assert {row["sku"]: int(row["pieces"]) for row in proposal["lines"]} == expected
    assert len(proposal["lines"]) == len(expected)
    assert all(row["unit"] == "EA" and row["revision"] is None for row in proposal["lines"])
    assert {"supplier_id", "purchase_order_id", "approved_at"}.isdisjoint(proposal)
    assert draft(bearer=None)[0] == 401 and draft(bearer=reader)[0] == 403
    assert draft(scope=(OTHER_TENANT, EXTERNAL_ORGANIZATION))[0] == 404

    def published():
        _, _, status = handoff()
        return status and status["event_id"] == sent["event_id"] and status["state"] == "published"

    eventually(published, "broker acknowledgement recorded")
    message = js_query("MSG.GET", {"last_by_subj": SUBJECT})["message"]
    envelope = json.loads(base64.b64decode(message["data"]))
    assert envelope["messageId"] == sent["event_id"]
    assert envelope["causationId"] == envelope["correlationId"] == plan["source_event_id"]
    assert {row["sku"]: int(row["pieces"]) for row in envelope["payload"]["buyRows"]} == expected

    # Fixture-only: recreate the outbox state after broker ack but before the
    # local marker write; wait beyond the broker's deduplication window.
    manage("stop", "inventory-plan-relay")
    try:
        selector = {"_id": {"tenantId": TENANT, "organizationId": ORGANIZATION,
                            "orderId": plan["order_id"], "lineId": plan["line_id"]},
                    "handoff.event_id": sent["event_id"], "handoff.pending": False}
        assert mongo_eval(COMPOSE, "print(db.getSiblingDB('inventory').material_plans.updateOne("
                          f"{json.dumps(selector)}, {{$set: {{'handoff.pending': true}},"
                          "$unset: {'handoff.published_at': ''}}).modifiedCount)") == 1
        sleep(2.1)
    finally:
        manage("start", "inventory-plan-relay")
    eventually(published, "broker acknowledgement after replay")
    replay = js_query("MSG.GET", {"last_by_subj": SUBJECT})["message"]
    assert replay["seq"] != message["seq"] and replay["data"] == message["data"]
    eventually(lambda: acked(replay["seq"]), "Procurement ack after replay")
    check_business_replays(envelope)
    manage("restart", "procurement")
    wait_until_ready(PROCUREMENT)
    assert draft()[2] == proposal and handoff(send=True)[2]["event_id"] == sent["event_id"]
    print("Pending handoff survived outage; one exact draft survived replay, rejected conflicts and foreign scope")
    if os.getenv("ERP4US_PURCHASE_TERMS_E2E") == "1":
        from procurement_terms import exercise as exercise_terms
        exercise_terms(proposal)
