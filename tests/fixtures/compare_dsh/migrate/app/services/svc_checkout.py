"""Client for the checkout service."""

from app import util


BASE = "https://checkout.internal.example.com"
TIMEOUT_MS_0 = 500


def load():
    r0 = util.fetch_json(BASE + "/v3/limits", TIMEOUT_MS_0)
    return [r0]
