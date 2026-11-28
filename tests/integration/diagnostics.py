"""Probe the real application endpoints before, during and after a Compose stop."""
import json
import os
import subprocess
from time import monotonic, sleep
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen
from uuid import uuid4

from delivery import broker_probe
from identity_fixture import token
from smoke import env, request


ORIGIN = f"http://127.0.0.1:{os.getenv('ERP4US_DIAGNOSTICS_PORT', '8091')}"
COMPOSE = ["docker", "compose", "-f", "compose.yaml", "-f", "compose.delivery.yaml",
           "-f", "compose.manufacturing.yaml", "-f", "compose.diagnostics.yaml"]
HEARTBEAT = "v1.diagnostics.system.article-delivery.event.service.heartbeat.v1"
FOREIGN_HEARTBEATS = "v1.diagnostics.system.*.event.service.heartbeat.v1"


def query(credential=None):
    headers = {} if credential is None else {"Authorization": f"Bearer {credential}"}
    try:
        with urlopen(Request(ORIGIN + "/v1/admin/application-health", headers=headers), timeout=8) as response:
            assert response.headers["Cache-Control"] == "no-store"
            return response.status, json.load(response)
    except HTTPError as error:
        return error.status, None


def wait(credential, desired, matches=lambda _: True):
    deadline = monotonic() + 30
    while monotonic() < deadline:
        try:
            status, report = query(credential)
        except URLError:
            sleep(0.5)
            continue
        assert status == 200
        states = {entry["service"]: entry["readiness"] for entry in report["application_checks"]}
        if all(states[name] == state for name, state in desired.items()) and matches(report):
            return report
        sleep(0.5)
    raise RuntimeError(f"Application checks did not transition to {desired}")


def check_broker_permissions():
    subjects = (
        "$JS.API.STREAM.MSG.GET.ERP4US_BUSINESS_EVENTS_V1",
        f"$JS.API.STREAM.DELETE.DIAG_NOT_A_REAL_STREAM_{uuid4().hex}",
        "$JS.API.CONSUMER.MSG.NEXT.ERP4US_BUSINESS_EVENTS_V1.article-receipts",
        "$JS.API.CONSUMER.MSG.NEXT.ERP4US_BUSINESS_EVENTS_V1.inventory-sales-v1",
        "$JS.API.CONSUMER.MSG.NEXT.ERP4US_BUSINESS_EVENTS_V1.procurement-material-plans-v1",
        "v1.config.system.article-delivery.event.heartbeat.changed.v1",
        HEARTBEAT,
    )
    # Test the delete ACL without targeting a real stream.
    output = broker_probe(COMPOSE, "platform_diagnostics", env["DEV_NATS_DIAGNOSTICS_PASSWORD"], [
        "SUB _INBOX.article-consumer.> 1", f"SUB {FOREIGN_HEARTBEATS} 2",
        *(command for subject in subjects for command in (f"PUB {subject} 1", "!")),
    ])
    for denied in (*subjects, "_INBOX.article-consumer.>", FOREIGN_HEARTBEATS):
        assert any("Permissions Violation" in line and denied in line for line in output.splitlines()), denied


def heartbeat(credential, state, *, different_from=None):
    def matches(report):
        observation = report["article_relay_heartbeat"]
        return observation["state"] == state and (
            different_from is None or observation["instance_id"] != different_from)

    return wait(credential, {}, matches)["article_relay_heartbeat"]


def check_heartbeat(credential):
    if os.getenv("COMPOSE_PROJECT_NAME") in (None, "", "erp4us", "erp4us-preview"):
        raise RuntimeError("Heartbeat test needs a named isolated Compose project")
    baseline = heartbeat(credential, "fresh")
    delay = baseline["interval_seconds"]
    assert isinstance(delay, int) and delay > 0
    assert 0 <= baseline["age_seconds"] < delay * 3
    assert baseline["instance_id"]

    try:
        subprocess.run([*COMPOSE, "stop", "article-relay"], check=True, timeout=40)
        assert heartbeat(credential, "unavailable") == {
            "state": "unavailable", "interval_seconds": None, "instance_id": None, "age_seconds": None,
        }
    finally:
        subprocess.run([*COMPOSE, "start", "article-relay"], check=True, timeout=40)
    resumed = heartbeat(credential, "fresh", different_from=baseline["instance_id"])
    assert resumed["instance_id"] and resumed["interval_seconds"] == delay

    result = subprocess.run(
        [*COMPOSE, "run", "--rm", "--no-deps", "-T", "article-config-reader"],
        capture_output=True, text=True, check=True, timeout=25,
    )
    config = json.loads(result.stdout)
    assert config["heartbeatSeconds"] == delay

    def set_interval(expected, seconds):
        subprocess.run(
            [*COMPOSE, "exec", "-T", "article-relay", "/usr/local/bin/article-delivery",
             "set-heartbeat", str(expected), str(seconds)], check=True, timeout=25,
        )

    set_interval(config["revision"], 0)
    try:
        assert heartbeat(credential, "disabled") == {
            "state": "disabled", "interval_seconds": 0, "instance_id": None, "age_seconds": None,
        }
    finally:
        set_interval(config["revision"] + 1, delay)
    assert heartbeat(credential, "fresh")["interval_seconds"] == delay


if __name__ == "__main__":
    credential = token(subject="synthetic-platform")
    before = wait(credential, {"master-data": "ready", "manufacturing": "ready", "jetstream": "ready"},
                  lambda report: all(report["jetstream"][key] is not None for key in ("stream", "consumer")))
    # Neither business durable has been installed in this application-readiness stack yet.
    assert before["jetstream"]["pipeline_consumers"] == {
        "inventory_sales": None, "procurement_material_plans": None,
    }
    assert query()[0] == 401
    assert query(token())[0] == 403  # Tenant writer is not a platform monitor.
    assert request("/v1/partners", bearer=credential)[0] == 403
    check_broker_permissions()
    count = before["jetstream"]["stream"]["messages"]
    article = json.dumps({"sku": f"DIAG-{uuid4().hex[:12].upper()}", "name": "Diagnostic fixture"}).encode()
    assert request("/v1/articles:register", article)[0] == 201
    wait(credential, {"jetstream": "ready"},
         lambda report: (report["jetstream"]["stream"] or {}).get("messages", 0) > count)
    try:
        subprocess.run([*COMPOSE, "stop", "manufacturing"], check=True, timeout=40)
        down = wait(credential, {"manufacturing": "unreachable", "master-data": "ready"})
        assert down["observed_at_unix_ms"] > before["observed_at_unix_ms"]
    finally:
        subprocess.run([*COMPOSE, "start", "manufacturing"], check=True, timeout=40)
    wait(credential, {"manufacturing": "ready"})
    try:
        subprocess.run([*COMPOSE, "stop", "nats"], check=True, timeout=40)
        down = wait(credential, {"jetstream": "unreachable", "manufacturing": "ready"})
        assert down["jetstream"] == {"stream": None, "consumer": None, "pipeline_consumers": {
            "inventory_sales": None, "procurement_material_plans": None,
        }}
        assert down["article_relay_heartbeat"]["state"] == "unavailable"
    finally:
        subprocess.run([*COMPOSE, "start", "nats"], check=True, timeout=40)
    wait(credential, {"jetstream": "ready"},
         lambda report: (report["jetstream"]["stream"] or {}).get("messages", 0) > count)
    check_heartbeat(credential)
    print("Platform-only readiness, live counters, heartbeat/recovery and broker ACLs passed")
