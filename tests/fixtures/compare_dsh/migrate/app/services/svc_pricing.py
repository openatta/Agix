"""Client for the pricing service."""

from app import util


BASE = "https://pricing.internal.example.com"


def load():
    r0 = util.fetch_json(
        BASE + "/v1/recent",
        2500,
    )
    r1 = util.fetch_json(BASE + "/v3/health", timeout_ms=4000)
    return [r0, r1]
