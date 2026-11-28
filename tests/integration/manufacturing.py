"""Verify scoped BOM requirements through the running Manufacturing API."""
import json
import os
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from identity_fixture import EXTERNAL_ORGANIZATION, ORGANIZATION, OTHER_TENANT, TENANT, token
from smoke import wait_until_ready


URL = f"http://127.0.0.1:{os.getenv('ERP4US_MANUFACTURING_PORT', '8090')}"
PRODUCTS = "/v1/manufacturing/products"
PLAN = "/v1/manufacturing/requirements:preview"


def request(path, *, credential=None, tenant=TENANT, organization=ORGANIZATION, data=None):
    headers = {"X-Erp4us-Tenant": tenant, "X-Erp4us-Organization": organization}
    if credential is not None:
        headers["Authorization"] = f"Bearer {credential}"
    if data is not None:
        headers["Content-Type"] = "application/json"
    try:
        with urlopen(Request(URL + path, headers=headers, data=data), timeout=10) as response:
            return response.status, json.load(response)
    except HTTPError as error:
        return error.status, None


def main():
    wait_until_ready(URL)
    writer = token()
    assert request(PRODUCTS)[0] == 401
    assert request(PRODUCTS, credential=token(subject="synthetic-reader"))[0] == 403
    assert request(PRODUCTS, credential=writer, tenant=OTHER_TENANT, organization=EXTERNAL_ORGANIZATION)[0] == 403
    status, products = request(PRODUCTS, credential=writer)
    assert status == 200
    assert [(product["sku"], product["price_eur"]) for product in products] == [
        ("NM-1000", "2490.00"), ("NM-2000", "890.00"),
    ]
    command = {"sku": "NM-1000", "revision": "A", "pieces": "10"}
    status, result = request(PLAN, credential=writer, data=json.dumps(command).encode())
    assert status == 200
    assert {part["sku"]: part["pieces"] for part in result["materials"]} == {
        "NM-CABLE": "10", "NM-COUPLING": "10", "NM-ENDPLATE": "20",
        "NM-MOTOR": "10", "NM-PACK": "10", "NM-PROFILE": "20",
        "NM-RAIL": "20", "NM-SCREW": "120", "NM-SENSOR": "20", "NM-SPINDLE": "10",
    }
    assert [row["sku"] for row in result["tree"] if row["children"]] == ["NM-1100", "NM-1200"]
    assert request(PLAN, credential=writer, data=json.dumps(command | {"revision": "B"}).encode())[0] == 404
    assert request(PLAN, credential=writer, data=json.dumps(command | {"pieces": "1.5"}).encode())[0] == 422
    print("Manufacturing availability, scoped catalog and exact BOM explosion checks passed")


if __name__ == "__main__":
    main()
