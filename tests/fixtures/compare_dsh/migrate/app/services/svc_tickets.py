"""Client for the tickets service."""

from app import util


BASE = "https://tickets.internal.example.com"


def load():
    r0 = util.fetch_json(BASE + "/v2/items", timeout_ms=500)
    return [r0]
