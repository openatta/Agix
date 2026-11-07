"""Client for the catalog service."""

from app import util


BASE = "https://catalog.internal.example.com"


def load():
    r0 = util.fetch_json(BASE + "/v2/items", timeout_ms=800)
    r1 = util.fetch_json(BASE + "/v1/health", timeout_ms=2000)
    return [r0, r1]
