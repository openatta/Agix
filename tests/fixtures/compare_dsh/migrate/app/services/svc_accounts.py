"""Client for the accounts service."""

from app import util


BASE = "https://accounts.internal.example.com"


def load():
    r0 = util.fetch_json(BASE + "/v1/items", 500)
    return [r0]
