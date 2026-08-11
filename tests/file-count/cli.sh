#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# Exercises the file-count worker: a file counts as 1, a tree recurses over its
# children through server-resolved map-then continuations (with itself on both
# sides) and sums the counts — so it totals a tree's leaf files, exercising the
# promise pipeline end to end. The fixture tree/ holds 5 files across nested
# dirs.
#
# The worker is a TEST FIXTURE, not a std entry: this test carries its source
# (./worker.rs) and builds it with std/rustc — memoized, so the compile
# happens once per source edit, not per run.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; }   # epoch milliseconds
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

echo "== build the fixture worker from its source ==" >&2
# No --runner: rustc DEPENDS on the runner pool (std/rustc/DEPS) and curries
# the built binary onto it itself, so a caller says only what it is building.
builder=$("$CAOS_CLI" curry DEEP-DEPS/rustc --)
"$CAOS_CLI" run "$builder" img -- --src:@=test/worker.rs
commit "built file-count"
worker=$(git rev-parse HEAD:img)

echo "== a whole tree totals its leaf files ==" >&2
t0=$(ms); n=$("$CAOS_CLI" run "$worker" -- --in:@=test/tree); t1=$(ms)
[ "$n" = "5" ] || fail "expected 5 leaf files, got: $n"
echo "  ok: tree -> 5" >&2

echo "== a single file counts as 1 ==" >&2
t2=$(ms); n=$("$CAOS_CLI" run "$worker" -- --in:@=test/tree/a.txt); t3=$(ms)
[ "$n" = "1" ] || fail "expected 1, got: $n"
echo "  ok: file -> 1" >&2

# The tree run is 11 cold jobs through the promise pipeline (root + 5 children
# + then-steps); the file run is 1. Both uncached (fresh salt per tests/run.sh).
echo "file-count perf (ms):" >&2
echo "  tree=$((t1 - t0))  file=$((t3 - t2))" >&2
echo "file-count: ALL PASS" >&2
