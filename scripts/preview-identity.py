"""Provision real, local-only Keycloak login for the manufacturing preview."""
import json
import os
from pathlib import Path
import secrets
import ssl
import subprocess
import sys
import time
from urllib.error import URLError
from urllib.request import HTTPSHandler, ProxyHandler, build_opener


ROOT = Path(__file__).resolve().parents[1]
DEMO = ROOT / ".erp4us-preview"
ISSUER = "https://127.0.0.1:9443/realms/erp4us-preview"
ORIGIN = "http://127.0.0.1:4200"
CLIENT = "erp4us-workspace"
AUDIENCE = "erp4us-master-data"
NOVA = ("ddcae139-c1e8-47d2-8b48-349cb6f30ce5", "3e793fb5-75a7-45f6-8f66-45a58fcec674")
OTHER = ("a260610d-ebcf-4bb7-b871-9d221ace30a8", "2886c61e-85d8-4688-813a-b59c6641f9f8")
USERS = {"production": "142a290f-4bbc-438e-aefe-e3697a8bcd16", "auditor": "154474cf-c105-473e-9843-38dd023d8a39",
         "platform-monitor": "163494cf-c105-473e-9843-38dd023d8a39"}
APPROVERS = {
    "department-approver": ("f54dff7f-ac9b-4551-b7f1-c60b58dcb360", "department"),
    "budget-approver": ("0c875371-a771-4f09-b781-e040d251ab2a", "budget"),
}
BUDGET_PREVIEW = os.getenv("ERP4US_PREVIEW_BUDGET_APPROVALS") == "1"
ISSUANCE_PREVIEW = os.getenv("ERP4US_PREVIEW_PURCHASE_ISSUANCE") == "1"
PURCHASE_READ = "procurement.purchase-order.read"
PURCHASE_ISSUE = "procurement.purchase-order.issue"
if BUDGET_PREVIEW:
    USERS.update({name: subject for name, (subject, _) in APPROVERS.items()})
if BUDGET_PREVIEW and ISSUANCE_PREVIEW:
    USERS["purchase-issuer"] = "12f3837a-0083-4994-9177-1e8c368d5817"


def save(path, value, mode=0o600):
    pending = path.with_suffix(".new")
    pending.write_text(json.dumps(value, indent=2) + "\n")
    pending.chmod(mode)
    pending.replace(path)


def grant(scope, permissions):
    return {"tenant_id": scope[0], "organization_id": scope[1], "permissions": permissions}


def identity_policy():
    principals = {
        USERS["production"]: {"actor_id": USERS["production"], "grants": [grant(NOVA, [
            "manufacturing.plan.read", "master-data.article.read", "master-data.article.register",
            "master-data.partner.read", "master-data.unit.read", "sales.order.read", "sales.order.draft.create",
            "sales.order.confirm", "inventory.reservation.read",
            "inventory.material-plan.read", "inventory.material-plan.create",
            "inventory.material-plan.send", "procurement.proposal.read",
            "procurement.proposal.terms.write",
        ])]},
        USERS["auditor"]: {"actor_id": USERS["auditor"], "grants": [grant(OTHER, ["master-data.article.read"])]},
        USERS["platform-monitor"]: {"actor_id": USERS["platform-monitor"], "grants": [],
                                    "platform_permissions": ["platform.health.read"]},
    }
    for username, (subject, stage) in APPROVERS.items():
        if username in USERS:
            principals[subject] = {"actor_id": subject, "grants": [grant(NOVA, [
                "sales.order.read", "inventory.reservation.read", "inventory.material-plan.read",
                "procurement.proposal.read", f"procurement.approval.{stage}",
            ])]}
    if "purchase-issuer" in USERS:
        buyer = principals[USERS["production"]]["grants"][0]
        buyer["permissions"].append(PURCHASE_READ)
        issuer = USERS["purchase-issuer"]
        principals[issuer] = {"actor_id": issuer, "grants": [grant(NOVA, [
            "sales.order.read", "inventory.reservation.read", "inventory.material-plan.read",
            "procurement.proposal.read", PURCHASE_READ, PURCHASE_ISSUE,
        ])]}
    return {"issuer": ISSUER, "audience": AUDIENCE, "principals": principals}


def prepare():
    if ISSUANCE_PREVIEW and not BUDGET_PREVIEW:
        raise SystemExit("The purchase issuance preview requires ERP4US_PREVIEW_BUDGET_APPROVALS=1")
    if DEMO.exists():
        if not (DEMO / "credentials.json").is_file():
            raise SystemExit("Incomplete preview identity directory; inspect it before retrying")
        if set(json.loads((DEMO / "credentials.json").read_text())) != USERS.keys():
            raise SystemExit("Preview accounts differ; use the original options to preserve the existing preview")
        save(DEMO / "identity-policy.json", identity_policy(), 0o644)
        return
    DEMO.mkdir(mode=0o700)
    for name in ("provider", "realm"):
        directory = DEMO / name
        directory.mkdir()
        directory.chmod(0o755)
    certificate = DEMO / "provider/localhost.crt"
    key = DEMO / "provider/localhost.key"
    subprocess.run([
        "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "30",
        "-subj", "/CN=erp4us local preview", "-addext", "subjectAltName=IP:127.0.0.1",
        "-out", str(certificate), "-keyout", str(key),
    ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    # Docker mounts only the provider/realm subdirectories. Their 0700 parent
    # prevents local host users other than the operator from reading these files.
    certificate.chmod(0o644)
    key.chmod(0o644)
    credentials = {user: secrets.token_urlsafe(24) for user in USERS}
    save(DEMO / "realm/erp4us-preview-realm.json", {
        "realm": "erp4us-preview", "enabled": True, "registrationAllowed": False,
        "accessTokenLifespan": 900,
        "clients": [{"clientId": CLIENT, "protocol": "openid-connect", "publicClient": True,
                     "standardFlowEnabled": True, "implicitFlowEnabled": False,
                     "directAccessGrantsEnabled": False,
                     "redirectUris": [ORIGIN + "/auth/callback"], "webOrigins": [ORIGIN],
                     "attributes": {"pkce.code.challenge.method": "S256",
                                    "post.logout.redirect.uris": ORIGIN + "/local-access"},
                     "protocolMappers": [{"name": "erp4us resource audience", "protocol": "openid-connect",
                                          "protocolMapper": "oidc-audience-mapper", "config": {
                                              "included.custom.audience": AUDIENCE,
                                              "access.token.claim": "true", "id.token.claim": "false",
                                          }}]}],
        "users": [{"id": subject, "username": username, "enabled": True,
                   "firstName": "Demo", "lastName": username.title(),
                   "email": f"{username}@example.invalid", "emailVerified": True,
                   "credentials": [{"type": "password", "value": credentials[username], "temporary": False}]}
                  for username, subject in USERS.items()],
    }, 0o644)
    save(DEMO / "identity-policy.json", identity_policy(), 0o644)
    save(DEMO / "credentials.json", credentials)


def configure():
    context = ssl.create_default_context(cafile=DEMO / "provider/localhost.crt")
    opener = build_opener(ProxyHandler({}), HTTPSHandler(context=context))
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        try:
            with opener.open(ISSUER + "/protocol/openid-connect/certs", timeout=3) as response:
                data = json.load(response)
            if any(key.get("alg") == "RS256" and key.get("use") == "sig" for key in data["keys"]):
                save(DEMO / "issuer-jwks.json", data, 0o644)
                save(ROOT / "ui/public/demo-oidc.json", {"issuer": ISSUER, "clientId": CLIENT}, 0o644)
                return
        except (URLError, TimeoutError):
            time.sleep(1)
    raise SystemExit("Local preview identity provider did not become ready")


if __name__ == "__main__":
    os.umask(0o077)
    if sys.argv[1:] == ["prepare"]:
        prepare()
    elif sys.argv[1:] == ["configure"]:
        configure()
    else:
        raise SystemExit("Usage: preview-identity.py prepare|configure")
