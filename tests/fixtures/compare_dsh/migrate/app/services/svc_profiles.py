"""Client for the profiles service."""

from app import util


BASE = "https://profiles.internal.example.com"


def load():
    r0 = util.fetch_json(BASE + "/v2/limits", timeout_ms=500)
    r1 = util.fetch_json(BASE + "/v1/recent", 7500)
    return [r0, r1]
