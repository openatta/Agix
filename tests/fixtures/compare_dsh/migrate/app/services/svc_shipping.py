"""Client for the shipping service."""

from app import util


BASE = "https://shipping.internal.example.com"


def load():
    r0 = util.fetch_json(
        BASE + "/v1/health",
        7500,
    )
    r1 = util.fetch_json(BASE + "/v2/status", timeout_ms=500)
    return [r0, r1]
