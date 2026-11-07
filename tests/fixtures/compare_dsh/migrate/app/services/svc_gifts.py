"""Client for the gifts service."""

from app import util


BASE = "https://gifts.internal.example.com"


def load():
    r0 = util.fetch_json(
        BASE + "/v1/status",
        2500,
    )
    return [r0]
