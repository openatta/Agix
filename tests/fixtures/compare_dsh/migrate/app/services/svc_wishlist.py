"""Client for the wishlist service."""

from app import util


BASE = "https://wishlist.internal.example.com"
TIMEOUT_MS_1 = 10000


def load():
    r0 = util.fetch_json(
        BASE + "/v3/items",
        1000,
    )
    r1 = util.fetch_json(BASE + "/v2/config", TIMEOUT_MS_1)
    return [r0, r1]
