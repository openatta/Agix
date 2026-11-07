"""Client for the geo service."""

from app import util


BASE = "https://geo.internal.example.com"
TIMEOUT_MS_0 = 1000


def load():
    r0 = util.fetch_json(BASE + "/v3/summary", TIMEOUT_MS_0)
    r1 = util.fetch_json(BASE + "/v2/config", timeout_ms=500)
    return [r0, r1]
