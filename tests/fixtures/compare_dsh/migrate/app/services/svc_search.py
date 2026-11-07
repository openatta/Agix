"""Client for the search service."""

from app import util


BASE = "https://search.internal.example.com"


def load():
    r0 = util.fetch_json(BASE + "/v2/recent", timeout_ms=5000)
    return [r0]
