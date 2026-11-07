"""Client for the loyalty service."""

from app import util


BASE = "https://loyalty.internal.example.com"
TIMEOUT_MS_1 = 1500


def load():
    r0 = util.fetch_json(
        BASE + "/v3/limits",
        500,
    )
    r1 = util.fetch_json(BASE + "/v3/recent", TIMEOUT_MS_1)
    return [r0, r1]
