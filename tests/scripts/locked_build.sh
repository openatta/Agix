#!/usr/bin/env bash
# Verify the carrier builds: one carrier or none, never both.
#
# Some deployments must be able to state that the binary they ship contains no
# plugin execution path at all. That claim is only true if the plugin crates
# are absent from the dependency graph, so this checks the graph rather than
# trusting the feature flag.
#
# `-p daemon --no-default-features` is not interchangeable with
# `--workspace --no-default-features`: cargo unifies features across the
# graph, so any other member that depends on daemon with `plugins` on turns it
# back on. The locked artifact must be built for this one package alone.
#
# ## Why the separate target directory
#
# Cargo keeps a full set of artifacts per feature combination, and wasmtime is
# several gigabytes of them. Building this configuration into the ordinary
# `target/` leaves a second complete copy behind every single run — which is
# how a checkout quietly grows to fill a disk. It goes somewhere disposable
# instead, and is disposed of.

set -euo pipefail
cd "$(dirname "$0")/../.."

LOCKED_TARGET="${TMPDIR:-/tmp}/agix-locked-check-$$"
# Cleaned on every exit path, not just the happy one: a failed run is exactly
# when someone re-runs it, and twice the artifacts is the last thing they need.
trap 'rm -rf "$LOCKED_TARGET"' EXIT
export CARGO_TARGET_DIR="$LOCKED_TARGET"

echo "==> asserting the two carriers cannot be built together"
# The rule the feature layout exists to state. It is enforced by a
# `compile_error!` in `daemon/src/lib.rs`, and a compile-time guard nobody
# tests is a guard that gets deleted by whoever is fixing an unrelated build.
if cargo check -p daemon --features plugins >/dev/null 2>&1; then
  echo "FAIL: a build carrying both the script and plugin carriers was accepted" >&2
  echo "The guard in daemon/src/lib.rs is gone or unreachable." >&2
  exit 1
fi

echo "==> building daemon without either carrier"
cargo build -p daemon --no-default-features

echo "==> asserting no carrier machinery is in the dependency graph"
leaked=$(cargo tree -p daemon --no-default-features -e normal \
  | grep -Eo '\b(plugin|plugin-host|wasm-host|wasmtime|cranelift-codegen|script-host|rquickjs|rquickjs-core|rquickjs-sys) v' || true)
if [ -n "$leaked" ]; then
  echo "FAIL: carrier machinery is still linked into the locked build:" >&2
  echo "$leaked" >&2
  exit 1
fi

# The other build worth guaranteeing: plugins present, compiler absent.
#
# A plugin's components are compiled once by `agix-plugin-compile` at install,
# so the process that runs them never needs Cranelift. That is only true if
# Cranelift is actually gone — and it is easy to lose, because a single
# dependency asking for it puts it back. `cranelift-entity` and friends stay:
# they are arenas and bitsets, not a compiler.
echo "==> asserting the plugins-without-compiler build links no compiler"
compiler=$(cargo tree -p daemon --no-default-features --features plugins -e normal \
  | grep -Eo '\b(cranelift-codegen|wasmtime-internal-cranelift|wasmtime-cranelift) v' || true)
if [ -n "$compiler" ]; then
  echo "FAIL: a WebAssembly compiler is linked into the runtime-only build:" >&2
  echo "$compiler" >&2
  echo "Something enabled wasm-host/compile. Note that \`default-features = false\`" >&2
  echo "on an inherited workspace dependency is ignored — it must be set in" >&2
  echo "[workspace.dependencies]." >&2
  exit 1
fi

# The claim the package/carrier split exists to make: the default build reads,
# verifies, unpacks and manages packages, and contains no WebAssembly at all.
# It is the one people actually ship, and the one where "you cannot install a
# plugin here" used to be true for want of an engine most packages never need.
echo "==> asserting the default build carries packages but no WebAssembly"
graph=$(cargo tree -p daemon -e normal)
if ! grep -q 'plugin v' <<<"$graph"; then
  echo "FAIL: the default build cannot read a package; the plugin crate is not in its graph" >&2
  exit 1
fi
wasm=$(grep -Eo '(plugin-host|wasm-host|wasmtime|cranelift-codegen) v' <<<"$graph" || true)
if [ -n "$wasm" ]; then
  echo "FAIL: WebAssembly machinery is linked into the default build:" >&2
  echo "$wasm" >&2
  exit 1
fi

echo "==> running the daemon test suite in the locked configuration"
cargo test -p daemon --no-default-features

# The plugin carrier is no longer part of the default build, so `cargo test
# --workspace` does not reach it any more. Nothing else does either — which
# would leave the whole subsystem untested a week after it stopped being the
# default, and nobody would notice until a release.
echo "==> running the daemon test suite carrying the plugin carrier"
cargo test -p daemon --no-default-features --features plugin-compile

echo "==> the DSH bridge is a separate process and has its own suite"
if command -v node >/dev/null 2>&1; then
  (cd bridges/agix-dsh-bridge && node --test 'test/**/*.test.js' >/dev/null)
  echo "    bridge tests pass"
else
  echo "    skipped: node is not installed"
fi

echo "OK: locked build contains no plugin subsystem, and the default build no WebAssembly"
