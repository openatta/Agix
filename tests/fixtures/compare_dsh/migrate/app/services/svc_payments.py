"""Client for the payments service."""

from app import util


BASE = "https://payments.internal.example.com"
TIMEOUT_MS_2 = 5000
TIMEOUT_MS_0 = 10000


def load():
    r0 = util.fetch_json(BASE + "/v3/config", TIMEOUT_MS_0)
    r1 = util.fetch_json(BASE + "/v1/health", timeout_ms=2500)
    r2 = util.fetch_json(BASE + "/v3/items", TIMEOUT_MS_2)
    return [r0, r1, r2]
