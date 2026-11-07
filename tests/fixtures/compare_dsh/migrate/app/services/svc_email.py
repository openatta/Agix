"""Client for the email service."""

from app import util


BASE = "https://email.internal.example.com"
TIMEOUT_MS_0 = 800


def load():
    r0 = util.fetch_json(BASE + "/v2/health", TIMEOUT_MS_0)
    r1 = util.fetch_json(
        BASE + "/v1/config",
        1000,
    )
    return [r0, r1]
