#!/usr/bin/env bash
# Runs *inside* a bash worker (launched by tests/run.sh). The test directory is at
# /cas/args/test and builtins are at /cas/std/<name>, all in a real /cas.
#
# Exercises the file-count worker as fold's `post` algebra: a file counts as 1, a
# directory sums its children's counts, so `fold --post=file-count` over a tree
# totals its leaf files. The fixture tree/ holds 5 files across nested dirs.
set -euo pipefail
T=/cas/args/test
caos get -r "$T"

fail() { echo "FAIL: $*" >&2; exit 1; }

# Total the leaf files under <src> via fold + file-count. <tag> keeps the /cas
# paths distinct across calls (a /cas path can't be overwritten in place).
count() { # <tag> <src> -> echoes the count
  caos put "$2" "/cas/in-$1"
  caos run /cas/std/fold "/cas/n-$1" -- --post=/cas/std/file-count --in:@="/cas/in-$1"
  caos get -r "/cas/n-$1"
  cat "/cas/n-$1"
}

echo "== a whole tree totals its leaf files ==" >&2
n=$(count tree "$T/tree")
[ "$n" = "5" ] || fail "expected 5 leaf files, got: $n"
echo "  ok: tree -> 5" >&2

echo "== a single file counts as 1 ==" >&2
n=$(count one "$T/tree/a.txt")
[ "$n" = "1" ] || fail "expected 1, got: $n"
echo "  ok: file -> 1" >&2

echo "file-count: ALL PASS" >&2
