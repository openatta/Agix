"""Client for the reports service."""

from app import util


BASE = "https://reports.internal.example.com"
TIMEOUT_MS_2 = 1000
TIMEOUT_MS_1 = 1500
TIMEOUT_MS_0 = 3000


def load():
    r0 = util.fetch_json(BASE + "/v1/limits", TIMEOUT_MS_0)
    r1 = util.fetch_json(BASE + "/v2/config", TIMEOUT_MS_1)
    r2 = util.fetch_json(BASE + "/v2/items", TIMEOUT_MS_2)
    return [r0, r1, r2]
