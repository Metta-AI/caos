#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# Exercises the file-count worker: a file counts as 1, a tree recurses over its
# children through server-resolved map-then continuations (with itself on both
# sides) and sums the counts — so it totals a tree's leaf files, exercising the
# promise pipeline end to end. The fixture tree/ holds 5 files across nested
# dirs.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ms() { date +%s%3N; }   # epoch milliseconds

echo "== a whole tree totals its leaf files ==" >&2
t0=$(ms); n=$("$CAOS_CLI" run /cas/std/file-count -- --in:@=test/tree); t1=$(ms)
[ "$n" = "5" ] || fail "expected 5 leaf files, got: $n"
echo "  ok: tree -> 5" >&2

echo "== a single file counts as 1 ==" >&2
t2=$(ms); n=$("$CAOS_CLI" run /cas/std/file-count -- --in:@=test/tree/a.txt); t3=$(ms)
[ "$n" = "1" ] || fail "expected 1, got: $n"
echo "  ok: file -> 1" >&2

# The tree run is 11 cold jobs through the promise pipeline (root + 5 children
# + then-steps); the file run is 1. Both uncached (fresh salt per tests/run.sh).
echo "file-count perf (ms):" >&2
echo "  tree=$((t1 - t0))  file=$((t3 - t2))" >&2
echo "file-count: ALL PASS" >&2
