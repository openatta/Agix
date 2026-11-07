"""Client for the fraud service."""

from app import util


BASE = "https://fraud.internal.example.com"


def load():
    r0 = util.fetch_json(BASE + "/v2/status", 1000)
    return [r0]
