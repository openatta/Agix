"""Client for the refunds service."""

from app import util


BASE = "https://refunds.internal.example.com"
TIMEOUT_MS_1 = 7500


def load():
    r0 = util.fetch_json(BASE + "/v2/summary", timeout_ms=7500)
    r1 = util.fetch_json(BASE + "/v2/summary", TIMEOUT_MS_1)
    r2 = util.fetch_json(BASE + "/v2/config", 5000)
    return [r0, r1, r2]
