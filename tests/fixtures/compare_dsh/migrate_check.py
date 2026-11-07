"""Hidden checker for the `migrate` comparison case. Run as
`python3 migrate_check.py <project>`: every service must make exactly the calls it
made before, through `get_json` with the timeout in seconds, and `fetch_json` must be
gone. Prints one line per problem and exits non-zero if there is any."""

import importlib
import inspect
import json
import pathlib
import sys

project = pathlib.Path(sys.argv[1]).resolve()
sys.path.insert(0, str(project))
expected = json.loads((pathlib.Path(__file__).parent / "migrate_expected.json").read_text())
problems = []

from app import util  # noqa: E402

if hasattr(util, "fetch_json"):
    problems.append("app/util.py still defines fetch_json")
params = inspect.signature(util.get_json).parameters
if list(params) != ["url", "timeout_s"] or params["timeout_s"].kind is not inspect.Parameter.KEYWORD_ONLY:
    problems.append(f"get_json's signature changed: {inspect.signature(util.get_json)}")

for path in sorted(project.joinpath("app").rglob("*.py")) + sorted(project.joinpath("tests").rglob("*.py")):
    if "fetch_json" in path.read_text():
        problems.append(f"{path.relative_to(project)} still mentions fetch_json")

calls = []


def stub(url, *, timeout_s):
    calls.append([url, timeout_s])
    return {"url": url}


util.get_json = stub
for name, want in sorted(expected.items()):
    calls.clear()
    try:
        importlib.import_module(f"app.services.{name}").load()
    except Exception as e:
        problems.append(f"{name}: load() raised {e!r}")
        continue
    same = len(calls) == len(want) and all(
        u == wu and abs(float(t) - wt) < 1e-9 for (u, t), (wu, wt) in zip(calls, want)
    )
    if not same:
        problems.append(f"{name}: calls {calls}, want {want}")

for p in problems:
    print(p)
sys.exit(1 if problems else 0)
