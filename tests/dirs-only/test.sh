#!/usr/bin/env bash
# Runs *inside* a bash worker (launched by tests/run.sh). The test directory is at
# /cas/args/test and builtins are at /cas/std/<name>, all in a real /cas.
#
# Exercises the dirs-only worker, a `pre` algebra for fold that keeps only a
# node's directory children and drops its files. The fixture tree/ holds 6 files
# across nested dirs plus two files at the top.
#
# First directly: running dirs-only over tree/ must yield a tree of just its
# directory children (dirA, dirB), with their subtrees intact, and none of the
# top-level files. Then as a fold `pre`: with dirs-only filtering children, no
# file is ever reached as a leaf, so `--post=file-count` totals 0 — versus 6 for
# the structural fold that does descend into files.
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
# dirs-only filters one level — deeper filtering is fold's job, via recursion.
[ -f /cas/filtered/dirA/a1.txt ] || fail "dirA lost its contents"
[ -f /cas/filtered/dirB/subdir/s1.txt ] || fail "dirB lost its nested contents"
echo "  ok: only dirA, dirB survived, with subtrees intact" >&2

echo "== as a fold pre job: files are excluded from the fold ==" >&2

# Structural fold (no pre): file-count totals every leaf file in the tree.
caos run /cas/std/fold /cas/all -- \
  --post=/cas/std/file-count --in:@="$T/tree"
caos get -r /cas/all
all=$(cat /cas/all)
[ "$all" = "6" ] || fail "expected 6 leaf files without pre, got: $all"
echo "  ok: structural fold counts 6 files" >&2

# With dirs-only as pre: a file is never a child, so no file leaf is ever
# reached — file-count sums to 0 at every directory, hence 0 overall.
caos run /cas/std/fold /cas/dirsonly -- \
  --pre=/cas/std/dirs-only --post=/cas/std/file-count --in:@="$T/tree"
caos get -r /cas/dirsonly
d=$(cat /cas/dirsonly)
[ "$d" = "0" ] || fail "expected 0 with dirs-only pre, got: $d"
echo "  ok: dirs-only pre excludes all files (count 0)" >&2

echo "dirs-only: ALL PASS" >&2
