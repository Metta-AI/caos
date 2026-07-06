#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# Exercises the file-count worker as fold's `post` algebra: a file counts as 1, a
# directory sums its children's counts, so `fold --post=file-count` over a tree
# totals its leaf files. The fixture tree/ holds 5 files across nested dirs.
# The fold recurses through server-resolved map-then continuations, so this also
# exercises the promise pipeline end to end.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== a whole tree totals its leaf files ==" >&2
n=$("$CAOS_CLI" run /cas/std/fold -- --post=/cas/std/file-count --in:@=test/tree)
[ "$n" = "5" ] || fail "expected 5 leaf files, got: $n"
echo "  ok: tree -> 5" >&2

echo "== a single file counts as 1 ==" >&2
n=$("$CAOS_CLI" run /cas/std/fold -- --post=/cas/std/file-count --in:@=test/tree/a.txt)
[ "$n" = "1" ] || fail "expected 1, got: $n"
echo "  ok: file -> 1" >&2

echo "file-count: ALL PASS" >&2
