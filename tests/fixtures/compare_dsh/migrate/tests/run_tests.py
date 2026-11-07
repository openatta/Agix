"""Runs every service's load() against a stub HTTP layer; no network needed."""

import importlib
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))
from app import util  # noqa: E402

calls = []


def stub(url, *, timeout_s):
    calls.append(url)
    return {"url": url}


util.get_json = stub

services = sorted(p.stem for p in (pathlib.Path(util.__file__).parent / "services").glob("svc_*.py"))
failures = 0
for name in services:
    try:
        importlib.import_module(f"app.services.{name}").load()
    except Exception as e:
        failures += 1
        print(f"FAIL {name}: {e!r}")
print(f"{len(services) - failures}/{len(services)} services load")
sys.exit(1 if failures else 0)
