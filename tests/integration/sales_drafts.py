"""Scoped Sales drafts, exact snapshots and safe retry against live owners."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
import subprocess
from uuid import uuid4

from identity_fixture import EXTERNAL_ORGANIZATION, OTHER_TENANT, token
from smoke import request, wait_until_ready


ORIGIN = f"http://127.0.0.1:{os.getenv('ERP4US_SALES_PORT', '8092')}"
ORDERS = "/v1/sales/orders"
COMPOSE = [
    "docker", "compose", "-f", "compose.yaml", "-f", "compose.manufacturing.yaml",
    "-f", "compose.sales.yaml",
]


def post(document):
    return request(ORDERS, json.dumps(document).encode(), origin=ORIGIN)


def main():
    wait_until_ready(ORIGIN)
    status, _, partners = request("/v1/partners?role=customer&after=BZ")
    assert status == 200
    atlas = next(partner for partner in partners["items"] if partner["code"] == "CUS-ATLAS")
    status, _, suppliers = request("/v1/partners?role=supplier")
    assert status == 200 and suppliers["items"]
    draft = {"id": str(uuid4()), "customer_id": atlas["id"],
             "sku": "NM-1000", "product_revision": "A", "pieces": "10"}
    for change in ({"unit_price": "1.00"}, {"status": "confirmed"}):
        assert post({**draft, **change})[0] == 400
    for change in ({"pieces": "10.0"}, {"product_revision": "MISSING"},
                   {"customer_id": str(uuid4())}, {"customer_id": suppliers["items"][0]["id"]}):
        assert post({**draft, **change})[0] == 422

    with ThreadPoolExecutor(max_workers=2) as pool:
        outcomes = list(pool.map(post, [draft, draft]))
    assert sorted(outcome[0] for outcome in outcomes) == [200, 201]
    _, headers, saved = outcomes[0]
    assert headers["Cache-Control"] == "no-store"
    assert saved == outcomes[1][2] and saved["id"] == draft["id"]
    assert saved["revision"] == 1 and saved["status"] == "draft"
    assert saved["customer"]["profile"] == atlas["profile"]
    assert saved["customer"]["id"] == atlas["id"]
    assert (saved["currency"], saved["total"]) == ("EUR", "24900.00")
    assert saved["line"]["pieces"] == "10" and saved["line"]["unit"] == "EA"
    assert saved["line"]["unit_price"] == "2490.00"
    assert post({**draft, "pieces": "11"})[0] == 409
    detail = f"{ORDERS}/{draft['id']}"
    assert request(detail, origin=ORIGIN)[2] == saved
    assert request(detail, bearer=None, origin=ORIGIN)[0] == 401
    assert request(detail, bearer=token(subject="synthetic-reader"), origin=ORIGIN)[0] == 403
    foreign = (OTHER_TENANT, EXTERNAL_ORGANIZATION)
    assert request(detail, scope=foreign, origin=ORIGIN)[0] == 404
    assert request(ORDERS, scope=foreign, origin=ORIGIN)[2] == {"items": [], "next_after": None}
    assert request(ORDERS, json.dumps(draft).encode(), scope=foreign, origin=ORIGIN)[0] == 403

    second = {**draft, "id": str(uuid4()), "pieces": "1"}
    assert post(second)[0] == 201
    first, second = sorted((draft["id"], second["id"]))
    status, _, page = request(f"{ORDERS}?limit=1&after={first}", origin=ORIGIN)
    assert status == 200 and page["items"][0]["id"] > first
    assert page["items"][0]["id"] <= second

    try:
        subprocess.run([*COMPOSE, "stop", "manufacturing"], check=True, timeout=40)
        assert post(draft)[2] == saved  # Retry preserves the captured originals, not new owner data.
        assert post({**draft, "id": str(uuid4())})[0] == 503
    finally:
        subprocess.run([*COMPOSE, "start", "manufacturing"], check=True, timeout=40)
    subprocess.run([*COMPOSE, "restart", "sales"], check=True, timeout=40)
    wait_until_ready(ORIGIN)
    assert request(detail, origin=ORIGIN)[2] == saved
    print("Sales exact snapshots, concurrent retry, pagination, tenant isolation and restart checks passed")


if __name__ == "__main__":
    main()
