"""Client for the reviews service."""

from app import util


BASE = "https://reviews.internal.example.com"


def load():
    r0 = util.fetch_json(BASE + "/v2/recent", timeout_ms=500)
    r1 = util.fetch_json(
        BASE + "/v3/status",
        2000,
    )
    r2 = util.fetch_json(
        BASE + "/v2/summary",
        1500,
    )
    return [r0, r1, r2]
