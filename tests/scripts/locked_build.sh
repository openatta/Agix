#!/usr/bin/env bash
# Verify the locked build: no script carrier and no package layer in the
# dependency graph, and no WebAssembly runtime in any build.
#
# Some deployments must be able to state that the binary they ship contains no
# extension execution path at all. That claim is only true if the crates are
# absent from the dependency graph, so this checks the graph rather than
# trusting the feature flag.
#
# `-p daemon --no-default-features --features full` is not interchangeable with
# `--workspace --no-default-features`: cargo unifies features across the
# graph, so any other member that depends on daemon with `scripts` on turns it
# back on. The locked artifact must be built for this one package alone.
#
# It goes into a disposable target directory: cargo keeps a full set of
# artifacts per feature combination, and a second copy left in `target/` on
# every run is how a checkout quietly fills a disk.

set -euo pipefail
cd "$(dirname "$0")/../.."

LOCKED_TARGET="${TMPDIR:-/tmp}/agix-locked-check-$$"
trap 'rm -rf "$LOCKED_TARGET"' EXIT
export CARGO_TARGET_DIR="$LOCKED_TARGET"

echo "==> building daemon without the script carrier or the package layer"
cargo build -p daemon --no-default-features --features full

echo "==> asserting no extension machinery is in the locked dependency graph"
# QuickJS itself stays: `RunCode` (`ptc`, part of `full`) runs the model's own
# programs in it, which is not an extension. A deployment that wants no
# JavaScript engine at all builds without `ptc`.
leaked=$(cargo tree -p daemon --no-default-features --features full -e normal \
  | grep -Eo '\b(plugin|script-host) v' || true)
if [ -n "$leaked" ]; then
  echo "FAIL: extension machinery is still linked into the locked build:" >&2
  echo "$leaked" >&2
  exit 1
fi

echo "==> asserting the default build carries packages and no WebAssembly"
graph=$(cargo tree -p daemon -e normal)
if ! grep -q 'plugin v' <<<"$graph"; then
  echo "FAIL: the default build cannot read a package; the plugin crate is not in its graph" >&2
  exit 1
fi
wasm=$(cargo tree -p daemon --all-features -e normal \
  | grep -Eo '\b(wasmtime|wasmer|wasmi|cranelift-codegen) v' || true)
if [ -n "$wasm" ]; then
  echo "FAIL: a WebAssembly runtime is linked into a daemon build:" >&2
  echo "$wasm" >&2
  exit 1
fi

echo "==> running the daemon test suite in the locked configuration"
cargo test -p daemon --no-default-features --features full

echo "OK: the locked build contains no extension machinery, and no build any WebAssembly"
