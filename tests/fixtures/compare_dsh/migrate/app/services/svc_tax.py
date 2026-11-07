"""Client for the tax service."""

from app import util


BASE = "https://tax.internal.example.com"
TIMEOUT_MS_2 = 5000


def load():
    r0 = util.fetch_json(
        BASE + "/v2/summary",
        500,
    )
    r1 = util.fetch_json(BASE + "/v1/limits", 800)
    r2 = util.fetch_json(BASE + "/v3/summary", TIMEOUT_MS_2)
    return [r0, r1, r2]
