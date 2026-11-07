"""Client for the returns service."""

from app import util


BASE = "https://returns.internal.example.com"


def load():
    r0 = util.fetch_json(
        BASE + "/v3/recent",
        10000,
    )
    r1 = util.fetch_json(
        BASE + "/v2/health",
        4000,
    )
    return [r0, r1]
