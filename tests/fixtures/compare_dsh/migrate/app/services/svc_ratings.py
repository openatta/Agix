"""Client for the ratings service."""

from app import util


BASE = "https://ratings.internal.example.com"


def load():
    r0 = util.fetch_json(BASE + "/v1/status", 800)
    r1 = util.fetch_json(
        BASE + "/v3/summary",
        500,
    )
    return [r0, r1]
