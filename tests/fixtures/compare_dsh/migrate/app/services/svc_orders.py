"""Client for the orders service."""

from app import util


BASE = "https://orders.internal.example.com"
TIMEOUT_MS_1 = 1500


def load():
    r0 = util.fetch_json(
        BASE + "/v3/status",
        500,
    )
    r1 = util.fetch_json(BASE + "/v2/config", TIMEOUT_MS_1)
    return [r0, r1]
