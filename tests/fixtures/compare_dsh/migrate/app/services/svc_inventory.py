"""Client for the inventory service."""

from app import util


BASE = "https://inventory.internal.example.com"


def load():
    r0 = util.fetch_json(BASE + "/v1/status", 5000)
    r1 = util.fetch_json(
        BASE + "/v1/config",
        5000,
    )
    r2 = util.fetch_json(
        BASE + "/v3/config",
        250,
    )
    return [r0, r1, r2]
