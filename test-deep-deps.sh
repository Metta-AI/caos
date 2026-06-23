#!/usr/bin/env bash
# Integration test for the `deep-deps` worker (caos-worker-deep-deps).
#
# Deepens a small package DAG and checks three things:
#   A. correctness + DAG sharing — the output tree has the right shape, and a
#      package depended on by two parents is one shared node (same content).
#   B. caching short-circuits — re-running identical input spawns no worker
#      (the top-level `all` call is a cache hit), so 0 `cache miss` log lines.
#   C. Merkle incrementality — editing an UNRELATED package leaves every other
#      node byte-identical; editing a leaf changes exactly the nodes that reach
#      it. Proven by content (diff -r), since the deepen driver re-runs on any
#      map edit by design and so raw miss-counts are noisy (reported as info).
#
# Requires the dev daemons running (`tilt up`): the caos server :9090 (storage +
# compute), redis, registry — and a docker the compute server can reach.
set -euo pipefail
cd "$(dirname "$0")"

# Host-side caos: build the client and point it at the dev daemons. deep-deps is
# written as a fold, so both images must be loaded.
echo "building caos client + loading deep-deps/fold images..." >&2
nix build .#client -o result-client
nix run .#load-caos-worker-deep-deps >/dev/null
nix run .#load-caos-worker-fold >/dev/null
caos=$PWD/result-client/bin/client

# Storage and compute are one server now — a single URL covers both.
export CAOS_SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}
# CAS must live on an xattr-capable fs (caos records each path's hash in
# user.caos.hash); the repo's fs qualifies, /tmp may not.
CAS=$PWD/.caos-dev/test-cas
PKGS=$(mktemp -d)
rm -rf "$CAS"; mkdir -p "$CAS"
export CAOS_CAS_DIR=$CAS
trap 'rm -rf "$CAS" "$PKGS" "$SNAP"' EXIT
SNAP=$(mktemp -d)

IMG=docker://caos-worker-deep-deps:latest

fail() { echo "FAIL: $*" >&2; exit 1; }
misses_since() { docker logs --since "$1" caos-compute-server 2>&1 \
                   | grep -c "cache miss:" || true; }

# Fixture: a -> {b,c}, b -> {d}, c -> {d}, d -> {}.
mkdir -p "$PKGS"/{a,b,c,d}
printf 'b\nc\n' > "$PKGS/a/DEPS"
printf 'd\n'    > "$PKGS/b/DEPS"
printf 'd\n'    > "$PKGS/c/DEPS"
: > "$PKGS/d/DEPS"

# Put the fixture, deepen every package, and materialize the result tree.
run() {
  rm -rf "$CAS/pkgs" "$CAS/out"
  "$caos" put "$PKGS" "$CAS/pkgs" >/dev/null
  "$caos" run "$IMG" "$CAS/out" -- --mode=all --packages="$CAS/pkgs" >/dev/null
  "$caos" get -r "$CAS/out" >/dev/null
}

echo "== Phase A: correctness + DAG sharing ==" >&2
run
[ -e "$CAS/out/a/DEEP-DEPS/b" ] || fail "a should depend on b"
[ -e "$CAS/out/a/DEEP-DEPS/c" ] || fail "a should depend on c"
[ -e "$CAS/out/b/DEEP-DEPS/d" ] || fail "b should depend on d"
[ -e "$CAS/out/a/DEPS" ]        && fail "DEPS should be dropped from nodes"
[ -n "$(ls -A "$CAS/out/d/DEEP-DEPS")" ] && fail "d should have no deep-deps"
diff -r "$CAS/out/b/DEEP-DEPS/d" "$CAS/out/c/DEEP-DEPS/d" >/dev/null \
  || fail "shared dep d should be one identical node under b and c"
echo "  ok: shape correct; DEPS dropped; d shared identically under b and c" >&2

# Snapshot the deepened nodes to compare against after edits.
cp -a "$CAS/out/." "$SNAP/"

echo "== Phase B: identical re-run is a full cache hit ==" >&2
sleep 1; since=$(date +%s)   # gap so the prior phase's logs fall before `since`
run
sleep 1
m=$(misses_since "$since")
[ "$m" -eq 0 ] || fail "identical re-run should be all hits, saw $m misses"
echo "  ok: 0 cache misses on identical re-run" >&2

echo "== Phase C1: editing an UNRELATED package leaves a,b,c,d untouched ==" >&2
mkdir -p "$PKGS/e"; : > "$PKGS/e/DEPS"
sleep 1; since=$(date +%s)   # gap so the prior phase's logs fall before `since`
run
sleep 1
for n in a b c d; do
  diff -r "$SNAP/$n" "$CAS/out/$n" >/dev/null \
    || fail "$n changed after editing an unrelated package"
done
[ -e "$CAS/out/e" ] || fail "e missing from output"
echo "  ok: a,b,c,d byte-identical; e added (misses: $(misses_since "$since"))" >&2

echo "== Phase C2: editing leaf d recomputes everything that reaches d ==" >&2
mkdir -p "$PKGS/x"; : > "$PKGS/x/DEPS"
printf 'x\n' > "$PKGS/d/DEPS"
sleep 1; since=$(date +%s)   # gap so the prior phase's logs fall before `since`
run
sleep 1
for n in a b c d; do
  diff -r "$SNAP/$n" "$CAS/out/$n" >/dev/null \
    && fail "$n should have changed when d changed"
done
[ -e "$CAS/out/d/DEEP-DEPS/x" ] || fail "d should now depend on x"
echo "  ok: a,b,c,d all recomputed (misses: $(misses_since "$since"))" >&2

echo "== Phase D: a dependency cycle is detected (by the compute server) ==" >&2
# Close a loop: d -> a, so a -> b -> d -> a (and a -> c -> d -> a). The fold
# recursion re-enters the same (fold image, args) and the compute server's
# run-cycle detection catches it.
rm -rf "$CAS/pkgs2" "$CAS/cyc"
printf 'a\n' > "$PKGS/d/DEPS"
"$caos" put "$PKGS" "$CAS/pkgs2" >/dev/null
if msg=$("$caos" run "$IMG" "$CAS/cyc" -- --mode=all --packages="$CAS/pkgs2" 2>&1); then
  fail "expected the cyclic graph to fail, but the run succeeded"
fi
echo "$msg" | grep -q "run cycle detected" || fail "no cycle reported; got: $msg"
echo "  ok: run failed with a run-cycle error" >&2

echo "ALL PASS" >&2
