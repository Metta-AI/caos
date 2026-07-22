#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a testenv worker — the suite's per-test job
# (tests/lib/run-nested.sh).
#
# Exercises the dirs-only worker, a filter that keeps only a node's directory
# children and drops its files. The fixture tree/ holds 6 files across nested
# dirs plus two files at the top. First directly: running dirs-only over tree/
# must yield a tree of just its directory children (dirA, dirB), with their
# subtrees intact. Then composed with file-count: filter first, then count the
# result (there's no `pre` — a different recursion set is built first, then
# recursed over).
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

echo "== dirs-only keeps directories, drops files ==" >&2
"$CAOS_CLI" run /cas/std/dirs-only filtered -- --in:@=test/tree
ls -la filtered >&2

[ -d filtered/dirA ] || fail "dirA (a directory) was dropped"
[ -d filtered/dirB ] || fail "dirB (a directory) was dropped"
[ ! -e filtered/top1.txt ] || fail "top1.txt (a file) was kept"
[ ! -e filtered/top2.txt ] || fail "top2.txt (a file) was kept"
# A kept directory keeps its original subtree (the contents, not just the name);
# dirs-only filters one level.
[ -f filtered/dirA/a1.txt ] || fail "dirA lost its contents"
[ -f filtered/dirB/subdir/s1.txt ] || fail "dirB lost its nested contents"
echo "  ok: only dirA, dirB survived, with subtrees intact" >&2

echo "== file-count sees every leaf file ==" >&2
all=$("$CAOS_CLI" run /cas/std/file-count -- --in:@=test/tree)
[ "$all" = "6" ] || fail "expected 6 leaf files, got: $all"
echo "  ok: file-count counts 6 files" >&2

echo "== counting the filtered tree sees only files under kept dirs ==" >&2
# The checkout above is untracked; commit it so the CLI can ingest it back.
commit "filtered tree"
d=$("$CAOS_CLI" run /cas/std/file-count -- --in:@=filtered)
# dirA has 2 files, dirB has 2 (one nested) — the top-level files are gone.
[ "$d" = "4" ] || fail "expected 4 files under kept dirs, got: $d"
echo "  ok: filter-then-count counts 4" >&2

echo "dirs-only: ALL PASS" >&2
