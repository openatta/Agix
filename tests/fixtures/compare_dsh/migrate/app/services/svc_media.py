"""Client for the media service."""

from app import util


BASE = "https://media.internal.example.com"


def load():
    r0 = util.fetch_json(BASE + "/v2/config", 7500)
    r1 = util.fetch_json(
        BASE + "/v2/config",
        4000,
    )
    return [r0, r1]
