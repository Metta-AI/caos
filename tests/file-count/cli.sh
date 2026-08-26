#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI)
# -----------------------------------------------------------
# This test's subject is NOT a static artifact a script could inspect on disk —
# it is a live recursive computation that only exists while it runs, and the
# ONLY way to make it run is to drive the tested client against the inner
# server. Nothing here can be shortcut with plain bash (contrast tests/std-lint,
# which reads checked-in files): the property under test is a behaviour of the
# CLI+server round-trip itself.
#
# What it pins down, and why the CLI is essential to each:
#
#   * curry + run round-trip. `$CAOS_CLI curry` builds the runner-pool base and
#     `$CAOS_CLI run` submits the request; the client walks the arg tree's git
#     closure and pushes it (via the seed alternate — see run-test.sh) so the
#     server can resolve it. A test that bypassed the client would never
#     exercise that closure-push / resolve path, which is exactly the client's
#     job.
#
#   * The map-then PROMISE PIPELINE. file-count on a tree does not return a
#     number; it records a continuation {in, map: file-count, then: file-count}
#     and exits. The SERVER fans out over the children in parallel, then calls
#     the ArgTree back with their results to sum them. That recursion is
#     server-resolved — it happens only because a `run` was submitted through
#     the client and the server drove the continuations. There is no on-disk
#     value to assert against; the "5" only comes into being by running it.
#
#   * LAZINESS / placeholder args. The worker reads `--in`/`--children` as
#     placeholders and only `caos get`s the bytes it needs — behaviour that is
#     meaningful only when a real client/server materialize args on demand.
#
# So the CLI here is essential, not incidental: it IS the thing that turns a
# fixture tree into a computed count. Asserting `tree -> 5` and `file -> 1` is
# asserting that the tested client can drive a real recursive job to completion.
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
builder=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/rustc)
"$CAOS_CLI" run img --base:hash="$builder" --src:@=test/worker.rs
commit "built file-count"
worker=$(git rev-parse HEAD:img)

echo "== a whole tree totals its leaf files ==" >&2
t0=$(ms); n=$("$CAOS_CLI" run --base:hash="$worker" --in:@=test/tree); t1=$(ms)
[ "$n" = "5" ] || fail "expected 5 leaf files, got: $n"
echo "  ok: tree -> 5" >&2

echo "== a single file counts as 1 ==" >&2
t2=$(ms); n=$("$CAOS_CLI" run --base:hash="$worker" --in:@=test/tree/a.txt); t3=$(ms)
[ "$n" = "1" ] || fail "expected 1, got: $n"
echo "  ok: file -> 1" >&2

# The tree run is 11 cold jobs through the promise pipeline (root + 5 children
# + then-steps); the file run is 1. Both uncached (fresh salt per tests/run.sh).
echo "file-count perf (ms):" >&2
echo "  tree=$((t1 - t0))  file=$((t3 - t2))" >&2
echo "file-count: ALL PASS" >&2
