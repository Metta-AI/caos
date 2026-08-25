#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (dev/run-test/run-test.sh).
#
# Exercises the dirs-only worker, a filter that keeps only a node's directory
# children and drops its files. The fixture tree/ holds 6 files across nested
# dirs plus two files at the top: running dirs-only over tree/ must yield a
# tree of just its directory children (dirA, dirB), with their subtrees
# intact — verified on the checked-out result, including its file count.
# (Worker-to-worker composition is covered by tests/run-then and
# tests/rust-worker.)
#
# The worker is a TEST FIXTURE, not a std entry: this test carries its source
# (./worker.rs) and builds it with std/rustc — memoized, so the compile
# happens once per source edit, not per run.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

echo "== build the fixture worker from its source ==" >&2
# No --runner: rustc DEPENDS on the runner pool (std/rustc/DEPS) and curries
# the built binary onto it itself, so a caller says only what it is building.
builder=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/rustc)
"$CAOS_CLI" run img --base:hash="$builder" --src:@=test/worker.rs
commit "built dirs-only"
worker=$(git rev-parse HEAD:img)

echo "== dirs-only keeps directories, drops files ==" >&2
"$CAOS_CLI" run filtered --base:hash="$worker" --in:@=test/tree
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

echo "== the filtered tree holds only files under kept dirs ==" >&2
d=$(find filtered -type f | wc -l)
# dirA has 2 files, dirB has 2 (one nested) — the top-level files are gone
# (the fixture holds 6 files total).
[ "$d" = "4" ] || fail "expected 4 files under kept dirs, got: $d"
echo "  ok: filtered tree holds 4 files" >&2

echo "dirs-only: ALL PASS" >&2
