"""Synthetic issuer for local HTTP tests only. Never deploy or share its keys."""

import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import time


DIRECTORY = Path(os.getenv("ERP4US_TEST_IDENTITY_DIR", ".dev-auth"))
KEY = DIRECTORY / "issuer-test.key"
ISSUER = "https://issuer.erp4us.test/realms/synthetic"
AUDIENCE = "erp4us-master-data"
TENANT = "ddcae139-c1e8-47d2-8b48-349cb6f30ce5"
OTHER_TENANT = "a260610d-ebcf-4bb7-b871-9d221ace30a8"
ORGANIZATION = "3e793fb5-75a7-45f6-8f66-45a58fcec674"
OTHER_ORGANIZATION = "8af68084-19d6-4e31-8203-f9f4708b9bee"
EXTERNAL_ORGANIZATION = "2886c61e-85d8-4688-813a-b59c6641f9f8"
ACTOR = "242a290f-4bbc-438e-aefe-e3697a8bcd16"
PURCHASE_READ = "procurement.purchase-order.read"
PURCHASE_ISSUE = "procurement.purchase-order.issue"


def b64(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=")


def token(*, subject="synthetic-writer", header_type="at+jwt", **overrides):
    now = int(time.time())
    header = {"alg": "RS256", "kid": "synthetic-rsa", "typ": header_type}
    claims = {"sub": subject, "iss": ISSUER, "aud": AUDIENCE, "iat": now,
              "exp": now + 300, **overrides}
    data = b".".join(b64(json.dumps(part).encode()) for part in (header, claims))
    signature = subprocess.check_output(
        ["openssl", "dgst", "-sha256", "-sign", KEY], input=data
    )
    return (data + b"." + b64(signature)).decode()


def grant(tenant, organization, *actions):
    return {"tenant_id": tenant, "organization_id": organization,
            "permissions": [f"master-data.article.{action}" for action in actions]}


def initialize():
    os.umask(0o077)
    DIRECTORY.mkdir()
    subprocess.run(["openssl", "genpkey", "-quiet", "-algorithm", "RSA", "-pkeyopt",
                    "rsa_keygen_bits:2048", "-pkeyopt", "rsa_keygen_pubexp:65537",
                    "-out", KEY], check=True)
    modulus = subprocess.check_output(["openssl", "rsa", "-in", KEY, "-noout", "-modulus"])
    jwks = {"keys": [{"kty": "RSA", "alg": "RS256", "use": "sig", "kid": "synthetic-rsa",
                      "n": b64(bytes.fromhex(modulus.decode().split("=")[1])).decode(),
                      "e": "AQAB"}]}
    policy = {"issuer": ISSUER, "audience": AUDIENCE, "principals": {
        "synthetic-writer": {"actor_id": ACTOR, "grants": [
            {
                **grant(TENANT, ORGANIZATION, "read", "register"),
                "permissions": [f"master-data.{resource}.{action}"
                                for resource in ("article", "unit", "partner")
                                 for action in ("read", "register")] + [
                                     "manufacturing.plan.read", "sales.order.read", "sales.order.draft.create",
                                     "sales.order.confirm", "inventory.reservation.read",
                                     "inventory.material-plan.read", "inventory.material-plan.create",
                                     "inventory.material-plan.send", "procurement.proposal.read",
                                      "procurement.proposal.terms.write", PURCHASE_READ],
            },
            grant(TENANT, OTHER_ORGANIZATION, "read"),
            {"tenant_id": OTHER_TENANT, "organization_id": EXTERNAL_ORGANIZATION,
             "permissions": ["master-data.article.read", "sales.order.read", "inventory.reservation.read",
                             "inventory.material-plan.read", "procurement.proposal.read"]},
        ]},
        "synthetic-reader": {"actor_id": "254474cf-c105-473e-9843-38dd023d8a39",
                              "grants": [grant(TENANT, ORGANIZATION, "read")]},
        "synthetic-sales-observer": {"actor_id": "9c8188df-9a86-4359-84a8-fd9df8eb0966",
                                     "grants": [{"tenant_id": TENANT, "organization_id": ORGANIZATION,
                                                 "permissions": ["sales.order.read"]}]},
        "synthetic-planning-reader": {
            "actor_id": "55842668-c776-45ae-8387-5418cb0306a4",
            "grants": [{"tenant_id": TENANT, "organization_id": ORGANIZATION,
                        "permissions": ["inventory.material-plan.read", "inventory.reservation.read"]}],
        },
        "synthetic-planning-no-bom": {
            "actor_id": "f07092b6-bdca-473f-bb14-4b76cc6f60d6",
            "grants": [{"tenant_id": TENANT, "organization_id": ORGANIZATION,
                        "permissions": ["inventory.material-plan.create"]}],
        },
        "synthetic-procurement-observer": {
            "actor_id": "280fd531-8e7a-4dea-a555-a12636f56c0b",
            "grants": [{"tenant_id": TENANT, "organization_id": ORGANIZATION,
                        "permissions": ["procurement.proposal.read"]}],
        },
        "synthetic-foreign-supplier-writer": {
            "actor_id": "9d588e80-4f8e-4e59-b10c-25fbd9feb4e7",
            "grants": [{"tenant_id": TENANT, "organization_id": OTHER_ORGANIZATION,
                        "permissions": ["master-data.partner.read", "master-data.partner.register"]}],
        },
        "synthetic-department-approver": {
            "actor_id": "f54dff7f-ac9b-4551-b7f1-c60b58dcb360",
            "grants": [{"tenant_id": TENANT, "organization_id": ORGANIZATION,
                        "permissions": ["procurement.proposal.read", "procurement.approval.department",
                                        "procurement.approval.budget"]}],
        },
        "synthetic-budget-approver": {
            "actor_id": "0c875371-a771-4f09-b781-e040d251ab2a",
            "grants": [
                {"tenant_id": TENANT, "organization_id": ORGANIZATION,
                 "permissions": ["procurement.proposal.read", "procurement.approval.budget"]},
                {"tenant_id": TENANT, "organization_id": OTHER_ORGANIZATION,
                 "permissions": ["procurement.proposal.read", "procurement.approval.budget"]},
            ],
        },
        "synthetic-dual-role-buyer": {
            # Subject alias for the original actor: grants do not let a
            # preparer approve or issue their own terms.
            "actor_id": ACTOR,
            "grants": [{"tenant_id": TENANT, "organization_id": ORGANIZATION,
                        "permissions": ["procurement.proposal.read", "procurement.approval.department",
                                        PURCHASE_READ, PURCHASE_ISSUE]}],
        },
        "synthetic-purchase-issuer": {
            "actor_id": "12f3837a-0083-4994-9177-1e8c368d5817",
            "grants": [
                {"tenant_id": tenant, "organization_id": org, "permissions": [PURCHASE_READ, PURCHASE_ISSUE]}
                for tenant, org in ((TENANT, ORGANIZATION), (TENANT, OTHER_ORGANIZATION),
                                    (OTHER_TENANT, EXTERNAL_ORGANIZATION))
            ],
        },
        "synthetic-platform": {"actor_id": "263494cf-c105-473e-9843-38dd023d8a39",
                               "grants": [], "platform_permissions": ["platform.health.read"]},
        "synthetic-registration": {"actor_id": "935c486c-37bc-4408-9629-7857100ba57c",
                                   "grants": [grant(TENANT, OTHER_ORGANIZATION, "register"),
                                              grant(OTHER_TENANT, EXTERNAL_ORGANIZATION, "register")]},
    }}
    for filename, contents in (("issuer-jwks.json", jwks), ("identity-policy.json", policy)):
        path = DIRECTORY / filename
        path.write_text(json.dumps(contents, indent=2) + "\n")
        # Compose mounts only these two non-secret files; the service runs as UID 10001.
        path.chmod(0o644)


if __name__ == "__main__":
    if sys.argv[1:] == ["token"]:
        print(token())
    elif not sys.argv[1:]:
        initialize()
    else:
        sys.exit("Usage: identity_fixture.py [token]")
