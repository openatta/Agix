#!/usr/bin/env bash
# What this checkout is using, and how much of it is dead.
#
# One clean build of this workspace is ~13 GB, and that is not waste: every
# integration test file is its own binary that statically links the whole
# graph — wasmtime and cranelift alone are half a gigabyte of rlib — so a
# test binary lands around 190 MB and there are dozens of them. Stripping
# barely moves it (~18 MB on macOS, where the debug info is not in the
# binary to begin with): that size is machine code.
#
# What does turn into waste is that cargo never collects. A superseded set
# stays on disk exactly as long as the live one, so the number to watch is
# not "how big is target" but "how much of it has nothing to do with the
# current build" — which is what this reports.

set -uo pipefail
cd "$(dirname "$0")/../.."

STALE_DAYS=${STALE_DAYS:-7}

echo "== disk =="
df -h . | tail -1
echo

echo "== build artifacts =="
total=0
for dir in target tests/fixtures/*/target; do
  [ -d "$dir" ] || continue
  size=$(du -sk "$dir" 2>/dev/null | cut -f1)
  total=$((total + size))
  printf '  %-45s %s\n' "$dir" "$(du -sh "$dir" 2>/dev/null | cut -f1)"
done
printf '  %-45s %sM\n' "(total)" "$((total / 1024))"
echo

# Untouched for a week is the practical definition of superseded: a live
# artifact is read on every build, so anything this old belongs to a feature
# combination, a dependency version, or a crate version that is no longer in
# play. This is the number `cargo sweep --time N` would reclaim.
if [ -d target ]; then
  echo "== untouched for ${STALE_DAYS}+ days (reclaimable) =="
  stale_bytes=$(find target -type f -mtime "+${STALE_DAYS}" -print0 2>/dev/null |
    xargs -0 -n 4000 stat -f%z 2>/dev/null |
    awk '{s += $1} END {print s + 0}')
  stale_files=$(find target -type f -mtime "+${STALE_DAYS}" 2>/dev/null | wc -l | tr -d ' ')
  printf '  %-45s %.1f GB in %s files\n' "target" \
    "$(awk -v b="$stale_bytes" 'BEGIN {print b / 1073741824}')" "$stale_files"
  echo
fi

echo "== what makes a second set appear =="
cat <<'TXT'
  1. Bumping the workspace version. Every crate's fingerprint changes, so the
     whole set is rebuilt and the previous one is orphaned in place. This is
     the usual cause of a sudden double-digit jump — clean after a bump.

  2. Building a different feature combination. Each keeps its own set:
       cargo build --workspace                      default, with the plugin compiler
       cargo build -p daemon --no-default-features  no plugin subsystem at all
       cargo build -p daemon --features plugins     plugins without the compiler
     (`locked_build.sh` builds the last two into a throwaway target dir of its
     own for exactly this reason, so it leaves nothing behind here.)

  3. Changing a dependency version or the toolchain. Same story.

  Note: several hashes behind one target name is NOT a staleness signal.
  A pristine tree has three `agixd-<hash>` executables — the binary, its
  own test harness, and the feature-varied build — because the hash encodes
  the feature set, not the age. Counting them was tried here and reported
  four "duplicates" on a freshly cleaned checkout, which is why the number
  above is measured by mtime instead.
TXT
echo

echo "== reclaiming =="
cat <<TXT
  Routine, keeps the live set:
    cargo sweep --time ${STALE_DAYS}      # cargo install cargo-sweep

  After a version bump, or when the number above is most of the total:
    cargo clean

  tests/fixtures/*/target belong to separate workspaces, so the top-level
  clean does not touch them:
    find tests/fixtures -name target -maxdepth 2 -type d -exec rm -rf {} +

  Test scratch directories, if a run was killed before its tempdirs dropped:
    rm -rf "\${TMPDIR:-/tmp}"/agix-* "\${TMPDIR:-/tmp}"/agix-*

  A full rebuild after a clean is about 1m10s on an M-series laptop, which is
  the whole cost of being wrong about what was still needed.
TXT
