"""Client for the invoices service."""

from app import util


BASE = "https://invoices.internal.example.com"
TIMEOUT_MS_1 = 4000


def load():
    r0 = util.fetch_json(
        BASE + "/v2/summary",
        5000,
    )
    r1 = util.fetch_json(BASE + "/v2/status", TIMEOUT_MS_1)
    return [r0, r1]
