#!/usr/bin/env bash
# Runs *inside* a bash worker (launched by tests/run.sh). The test directory is at
# /cas/args/test and builtins are at /cas/std/<name>, all in a real /cas.
#
# Exercises the dirs-only worker, a filter that keeps only a node's directory
# children and drops its files. The fixture tree/ holds 6 files across nested
# dirs plus two files at the top.
#
# First directly: running dirs-only over tree/ must yield a tree of just its
# directory children (dirA, dirB), with their subtrees intact, and none of the
# top-level files. Then alongside a structural fold: file-count over the whole
# tree still sees every leaf file (dirs-only filters one level when applied
# directly; it does not change what fold recurses into).
set -euo pipefail
T=/cas/args/test
caos get -r "$T"

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== dirs-only keeps directories, drops files ==" >&2
caos run /cas/std/dirs-only /cas/filtered -- --in:@="$T/tree"
caos get -r /cas/filtered
ls -la /cas/filtered >&2

[ -d /cas/filtered/dirA ] || fail "dirA (a directory) was dropped"
[ -d /cas/filtered/dirB ] || fail "dirB (a directory) was dropped"
[ ! -e /cas/filtered/top1.txt ] || fail "top1.txt (a file) was kept"
[ ! -e /cas/filtered/top2.txt ] || fail "top2.txt (a file) was kept"
# A kept directory keeps its original subtree (the contents, not just the name);
# dirs-only filters one level.
[ -f /cas/filtered/dirA/a1.txt ] || fail "dirA lost its contents"
[ -f /cas/filtered/dirB/subdir/s1.txt ] || fail "dirB lost its nested contents"
echo "  ok: only dirA, dirB survived, with subtrees intact" >&2

echo "== structural fold still counts every leaf file ==" >&2
caos run /cas/std/fold /cas/all -- \
  --post=/cas/std/file-count --in:@="$T/tree"
caos get -r /cas/all
all=$(cat /cas/all)
[ "$all" = "6" ] || fail "expected 6 leaf files, got: $all"
echo "  ok: structural fold counts 6 files" >&2

echo "== folding the filtered tree counts only files under kept dirs ==" >&2
# Composition without fold's old `pre`: filter first, then fold the result.
# dirA has 2 files, dirB has 2 (one nested) — the top-level files are gone.
caos run /cas/std/fold /cas/filteredcount -- \
  --post=/cas/std/file-count --in:@=/cas/filtered
caos get -r /cas/filteredcount
d=$(cat /cas/filteredcount)
[ "$d" = "4" ] || fail "expected 4 files under kept dirs, got: $d"
echo "  ok: filter-then-fold counts 4" >&2

echo "dirs-only: ALL PASS" >&2
