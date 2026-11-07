"""HTTP helpers shared by every service client."""

import json
import urllib.request


def fetch_json(url, timeout_ms):
    """Deprecated: use get_json. GET `url` and decode the JSON body; `timeout_ms` is in
    milliseconds."""
    return get_json(url, timeout_s=timeout_ms / 1000)


def get_json(url, *, timeout_s):
    """GET `url` and decode the JSON body, giving up after `timeout_s` seconds."""
    with urllib.request.urlopen(url, timeout=timeout_s) as response:
        return json.load(response)
