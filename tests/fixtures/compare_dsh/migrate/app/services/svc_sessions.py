"""Client for the sessions service."""

from app import util


BASE = "https://sessions.internal.example.com"


def load():
    r0 = util.fetch_json(
        BASE + "/v2/status",
        1000,
    )
    r1 = util.fetch_json(BASE + "/v2/health", timeout_ms=7500)
    return [r0, r1]
