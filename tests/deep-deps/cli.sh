#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set — normally INSIDE a testenv worker, as the suite's per-test job
# (tests/lib/run-nested.sh); tests/run.sh runs it on the host against the
# outer stack for interactive debugging.
#
# deep-deps turns a flat, name-keyed package map (each package a `DEPS` list, one
# dependency name per line) into a DAG of nodes: the output mirrors the input but
# each node carries a `DEEP-DEPS` subtree of its recursively-deepened direct deps
# (and drops its own DEPS). We check correctness + DAG sharing, incremental
# recompute on edits, and cycle detection. The fixture map (a -> {b,c}, b -> {d},
# c -> {d}, d -> {}) lives as real files under packages/. The recursion runs as
# server-resolved map-then promises, deps mapped in parallel.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
# The CLI ingests only git-tracked paths, so commit the map before each deepen.
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

# Deepen a package map (a dir) and check the result tree out.
deepen() { # <pkgs-dir> <out-dir>
  "$CAOS_CLI" run /cas/std/deep-deps "$2" -- --mode=all --packages:@="$1"
}

# A writable copy of the fixture map we edit across phases.
cp -R test/packages pkgs
chmod -R u+w pkgs
commit "phase A map"

echo "== Phase A: correctness + DAG sharing ==" >&2
deepen pkgs outA
A=outA
[ -e "$A/a/DEEP-DEPS/b" ] || fail "a should depend on b"
[ -e "$A/a/DEEP-DEPS/c" ] || fail "a should depend on c"
[ -e "$A/b/DEEP-DEPS/d" ] || fail "b should depend on d"
[ -e "$A/a/DEPS" ]        && fail "DEPS should be dropped from nodes"
[ -n "$(ls -A "$A/d/DEEP-DEPS")" ] && fail "d should have no deep-deps"
diff -r "$A/b/DEEP-DEPS/d" "$A/c/DEEP-DEPS/d" >/dev/null \
  || fail "shared dep d should be one identical node under b and c"
echo "  ok: shape correct; DEPS dropped; d shared identically under b and c" >&2

echo "== Phase B: editing an UNRELATED package leaves a,b,c,d untouched ==" >&2
mkdir -p pkgs/e; : > pkgs/e/DEPS
commit "phase B map"
deepen pkgs outB
for n in a b c d; do
  diff -r "$A/$n" "outB/$n" >/dev/null \
    || fail "$n changed after editing an unrelated package"
done
[ -e outB/e ] || fail "e missing from output"
echo "  ok: a,b,c,d byte-identical; e added" >&2

echo "== Phase C: editing leaf d recomputes everything that reaches d ==" >&2
mkdir -p pkgs/x; : > pkgs/x/DEPS
printf 'x\n' > pkgs/d/DEPS
commit "phase C map"
deepen pkgs outC
for n in a b c d; do
  diff -r "$A/$n" "outC/$n" >/dev/null \
    && fail "$n should have changed when d changed"
done
[ -e outC/d/DEEP-DEPS/x ] || fail "d should now depend on x"
echo "  ok: a,b,c,d all recomputed; d now reaches x" >&2

echo "== Phase D: a dependency cycle is detected (by the server) ==" >&2
# Close a loop: d -> a, so a -> b -> d -> a. The deepen recursion re-enters the
# same request and the server's run-cycle detection catches it.
printf 'a\n' > pkgs/d/DEPS
commit "phase D map"
if deepen pkgs outD 2>cyc.err; then
  fail "expected the cyclic graph to fail, but the run succeeded"
fi
grep -q "run cycle detected" cyc.err || fail "no cycle reported; got: $(cat cyc.err)"
echo "  ok: run failed with a run-cycle error" >&2

echo "deep-deps: ALL PASS" >&2
