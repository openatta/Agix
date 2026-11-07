"""Client for the coupons service."""

from app import util


BASE = "https://coupons.internal.example.com"
TIMEOUT_MS_1 = 10000


def load():
    r0 = util.fetch_json(
        BASE + "/v1/items",
        10000,
    )
    r1 = util.fetch_json(BASE + "/v3/config", TIMEOUT_MS_1)
    return [r0, r1]
