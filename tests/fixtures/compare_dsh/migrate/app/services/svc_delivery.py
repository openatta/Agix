"""Client for the delivery service."""

from app import util


BASE = "https://delivery.internal.example.com"


def load():
    r0 = util.fetch_json(BASE + "/v2/recent", 800)
    return [r0]
