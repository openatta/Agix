"""Client for the billing service."""

from app import util


BASE = "https://billing.internal.example.com"


def load():
    r0 = util.fetch_json(BASE + "/v3/summary", 2500)
    r1 = util.fetch_json(BASE + "/v1/status", timeout_ms=7500)
    return [r0, r1]
