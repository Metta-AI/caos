#!/usr/bin/env bash
# The caos test runner: fire THE SUITE JOB against the running stack and
# print its report. It never starts, updates, or restarts the stack — that's
# where agents live, and it embodies the old, known-good caos that builds and
# tests the edited tree; it only requires one (with a published std) and adds
# worker images to the engine store, which disturbs nothing running. The
# suite itself is a caos worker (tests/lib/suite.sh): it run-thens
# the workspace build (std/cargo — the old, known-good caos building the
# edited tree), fans out one job per tests/<name>/cli.sh (map-then, so
# parallelism is slot-bounded by the runner pool), and summarizes. Every
# level is cached: an unchanged test never re-runs, and an unchanged EVERYTHING
# is one suite-level cache hit. An agent inside caos fires the identical job —
# this script is just the host's front door.
#
# Jobs run unsalted by default so caching works across runs; export CAOS_SALT
# to force a re-run (e.g. to retry a flaky failure — failed verdicts are
# values and cache like results).
#
# Nothing is built on the host: the client is the installed one (like the
# stack, it's the old, known-good caos — a deploy artifact), and everything
# under test — binaries, worker images, the toolchain bake — is built by
# caos jobs from the workspace tree. No nix in the test path at all.
#
# Usage: tests/run.sh [name...]   Exits non-zero if any test fails.
# With names, the suite job runs just those tests (a filtered suite caches
# separately, but its per-test jobs share their cache with full runs — so
# `run.sh symlinks` after a full run is all hits, and vice versa).
set -uo pipefail
cd "$(dirname "$0")/.."

ONLY=("$@")
for t in "${ONLY[@]}"; do
  [ -f "tests/$t/cli.sh" ] || { echo "no such test: tests/$t/cli.sh" >&2; exit 2; }
done

# The client, like the stack, is part of the OLD, known-good caos — it only
# fires the suite job, and is REQUIRED, never built from the edited tree
# (the edited client is built and exercised INSIDE the suite: every test
# drives /pt/caos-cli from the caos-built bins). Take it from $CAOS_CLI,
# PATH (the devshell provides one), or the stack's own install (`caosd up`
# puts its client at $CAOS_DATA/bin — client and stack deploy together).
CAOS_DATA="${CAOS_DATA:-$PWD/.caos-data}"
CAOS_CLI=${CAOS_CLI:-$(command -v caos-cli || true)}
[ -n "$CAOS_CLI" ] || [ ! -x "$CAOS_DATA/bin/caos-cli" ] || CAOS_CLI=$CAOS_DATA/bin/caos-cli
[ -n "$CAOS_CLI" ] || {
  echo "tests/run.sh: no caos client found (CAOS_CLI, PATH, $CAOS_DATA/bin)." >&2
  echo "the stack's deploy installs one: nix run .#caosd -- up" >&2
  echo "(or enter the devshell: nix develop)" >&2
  exit 1
}
export CAOS_CLI

# The tester does NOT start, update, or restart the stack: the stack is
# where agents (and their conversations) live, and it embodies the OLD,
# known-good caos that builds and tests the edited tree — replacing it is a
# deploy decision, never a side effect of running tests. Require a working
# stack with a published std, and otherwise leave it alone.
git remote get-url caos >/dev/null 2>&1 || {
  echo "tests/run.sh: this repo needs a 'caos' remote naming the local server:" >&2
  echo "  git remote add caos http://localhost:9090" >&2
  exit 1
}
if [ -z "$(git ls-remote caos refs/caos/std 2>/dev/null | cut -f1)" ]; then
  echo "tests/run.sh: no running caos stack with a published std at" >&2
  echo "  $(git remote get-url caos)" >&2
  echo "start one (separately — this script never touches the stack) with:" >&2
  echo "  nix run .#caosd -- up" >&2
  exit 1
fi
OUT=$PWD/.caos-dev/run-all
rm -rf "$OUT" && mkdir -p "$OUT"

# ---------------------------------------------------------------------------
# The suite job. Every image the tests use is built IN the suite: the
# runner/bash/nix-builder images from pinned stock bases + the caos-built
# binaries (phase D1), and the cargo toolchain base by a nix bake inside the
# nix-builder worker (phase D2). Nothing image-shaped is produced here.
# ---------------------------------------------------------------------------
extra=()
# The real API key rides as an ordinary arg; stage 2 places it in
# chat-online's map child alone, so only that test re-keys when it rotates.
[ -n "${ANTHROPIC_API_KEY:-}" ] && extra+=(--api_key="$ANTHROPIC_API_KEY")
[ "${#ONLY[@]}" -gt 0 ] && extra+=(--only="${ONLY[*]}")

echo "== firing the suite job ==" >&2
# The suite's interface is a tool's interface: the workspace tree and the
# optional extras. Every downstream script comes from the workspace itself
# (caos-tools/*, tests/lib/*) — the suite tests exactly the harness the
# tree carries. (--script names the same suite.sh; the two move together.)
suite_hash=$(CAOS_SALT="${CAOS_SALT:-}" "$CAOS_CLI" run /cas/std/bash "$OUT/suite" -- \
  --script:@=tests/lib/suite.sh \
  --workspace:@=. \
  "${extra[@]}") \
  || { echo "suite job failed" >&2; exit 1; }

echo >&2
cat "$OUT/suite/report" >&2
# The COMPLETE record — every test's full output + inner-stack logs — is the
# suite result's `results/` tree, checked out under $OUT/suite and pinned on
# the server (the printed hash). Show a failing test's output tail here.
echo "  full results: $OUT/suite/results ($suite_hash)" >&2
for r in "$OUT"/suite/results/*/; do
  t=$(basename "$r")
  grep -q "^RUN-TEST: PASS" "$r/verdict" && continue
  { echo; echo "---- tests/$t (output tail; full record in results/$t) ----"
    tail -40 "$r/output"; } >&2
done
grep -q "^SUITE OK" "$OUT/suite/report"
