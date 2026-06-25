#!/usr/bin/env bash
# Runs *inside* a bash worker (launched by tests/run.sh). The test directory is at
# /cas/args/test and builtins are at /cas/std/<name>, all in a real /cas.
#
# deep-deps turns a flat, name-keyed package map (each package a `DEPS` list, one
# dependency name per line) into a DAG of nodes: the output mirrors the input but
# each node carries a `DEEP-DEPS` subtree of its recursively-deepened direct deps
# (and drops its own DEPS). We check correctness + DAG sharing, incremental
# recompute on edits, and cycle detection. The fixture map (a -> {b,c}, b -> {d},
# c -> {d}, d -> {}) lives as real files under packages/.
set -euo pipefail
T=/cas/args/test
caos get -r "$T"

fail() { echo "FAIL: $*" >&2; exit 1; }

# Deepen a package map (a dir) and materialize the result tree.
deepen() { # <src-dir> <pkgs-cas> <out-cas>
  caos put "$1" "$2"
  caos run /cas/std/deep-deps "$3" -- --mode=all --packages:@="$2"
  caos get -r "$3"
}

# A writable copy of the fixture map we edit across phases (fetched fixture files
# are read-only, so restore write before editing).
cp -r "$T/packages" /tmp/pkgs
chmod -R u+w /tmp/pkgs

echo "== Phase A: correctness + DAG sharing ==" >&2
deepen /tmp/pkgs /cas/pkgsA /cas/outA
A=/cas/outA
[ -e "$A/a/DEEP-DEPS/b" ] || fail "a should depend on b"
[ -e "$A/a/DEEP-DEPS/c" ] || fail "a should depend on c"
[ -e "$A/b/DEEP-DEPS/d" ] || fail "b should depend on d"
[ -e "$A/a/DEPS" ]        && fail "DEPS should be dropped from nodes"
[ -n "$(ls -A "$A/d/DEEP-DEPS")" ] && fail "d should have no deep-deps"
diff -r "$A/b/DEEP-DEPS/d" "$A/c/DEEP-DEPS/d" >/dev/null \
  || fail "shared dep d should be one identical node under b and c"
echo "  ok: shape correct; DEPS dropped; d shared identically under b and c" >&2

echo "== Phase B: editing an UNRELATED package leaves a,b,c,d untouched ==" >&2
mkdir -p /tmp/pkgs/e; : > /tmp/pkgs/e/DEPS
deepen /tmp/pkgs /cas/pkgsB /cas/outB
for n in a b c d; do
  diff -r "$A/$n" "/cas/outB/$n" >/dev/null \
    || fail "$n changed after editing an unrelated package"
done
[ -e /cas/outB/e ] || fail "e missing from output"
echo "  ok: a,b,c,d byte-identical; e added" >&2

echo "== Phase C: editing leaf d recomputes everything that reaches d ==" >&2
mkdir -p /tmp/pkgs/x; : > /tmp/pkgs/x/DEPS
printf 'x\n' > /tmp/pkgs/d/DEPS
deepen /tmp/pkgs /cas/pkgsC /cas/outC
for n in a b c d; do
  diff -r "$A/$n" "/cas/outC/$n" >/dev/null \
    && fail "$n should have changed when d changed"
done
[ -e /cas/outC/d/DEEP-DEPS/x ] || fail "d should now depend on x"
echo "  ok: a,b,c,d all recomputed; d now reaches x" >&2

echo "== Phase D: a dependency cycle is detected (by the server) ==" >&2
# Close a loop: d -> a, so a -> b -> d -> a. The fold recursion re-enters the
# same request and the server's run-cycle detection catches it.
printf 'a\n' > /tmp/pkgs/d/DEPS
caos put /tmp/pkgs /cas/pkgsD
if caos run /cas/std/deep-deps /cas/outD -- --mode=all --packages:@=/cas/pkgsD 2>/tmp/cyc; then
  fail "expected the cyclic graph to fail, but the run succeeded"
fi
grep -q "run cycle detected" /tmp/cyc || fail "no cycle reported; got: $(cat /tmp/cyc)"
echo "  ok: run failed with a run-cycle error" >&2

echo "deep-deps: ALL PASS" >&2
