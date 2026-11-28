"""Verify the seeded directory, bounded pages, and read denial over HTTP."""
from identity_fixture import EXTERNAL_ORGANIZATION, OTHER_TENANT, token
from smoke import request, wait_until_ready


def page(path):
    status, _, result = request("/v1/partners" + path)
    assert status == 200
    return result


if __name__ == "__main__":
    wait_until_ready()
    first = page("?role=customer&limit=2&after=BZ")
    assert [item["code"] for item in first["items"]] == ["CUS-ATLAS", "CUS-BLUEPEAK"]
    assert first["next_after"] == "CUS-BLUEPEAK"
    last = page("?role=customer&limit=2&after=CUS-BLUEPEAK")
    assert last["items"][0]["code"] == "CUS-WESTBRIDGE"
    atlas = first["items"][0]
    assert atlas["name"] == "Atlas Handling Technologies GmbH"
    assert atlas["profile"]["delivery_address"]["country"] == "DE"
    assert atlas["profile"]["payment_days"] == 30
    assert len([p for p in page("?role=supplier")["items"] if p["code"].startswith("SUP-")]) == 4
    assert request("/v1/partners?limit=51")[0] == 400
    assert request("/v1/partners", bearer=token(subject="synthetic-reader"))[0] == 403
    assert request("/v1/partners", scope=(OTHER_TENANT, EXTERNAL_ORGANIZATION))[0] == 403
    print("Scoped partners, stable pagination, seed replay and denial checks passed")
