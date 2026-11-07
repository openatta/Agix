"""Client for the notify service."""

from app import util


BASE = "https://notify.internal.example.com"


def load():
    r0 = util.fetch_json(BASE + "/v2/health", timeout_ms=10000)
    return [r0]
