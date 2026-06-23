#!/usr/bin/env bash
# Integration test for the `deep-deps` worker, driven through the built-ins
# (`/cas/std`). It also exercises the whole built-in path: populate the `refs/caos/std`
# tag, run the deep-deps built-in by `/cas/std/deep-deps`, and check that bumping
# `std` recomputes (since `std` is part of every cache key).
#
# Checks:
#   A. correctness + DAG sharing — right shape, and a package depended on by two
#      parents is one shared node.
#   B. caching short-circuits — an identical re-run spawns no worker (0 misses).
#   C. Merkle incrementality — editing an unrelated package leaves the others
#      byte-identical; editing a leaf changes exactly what reaches it.
#   E. bumping `caos/std` recomputes everything (std is in the key).
#   D. a dependency cycle is detected.
#
# Requires the dev daemons running (`tilt up`): the caos server :9090 (storage +
# compute), redis, registry — and a docker the server can reach. The server's
# object DB must be this repo's .git (so build-builtins.sh.s refs/caos/std is
# visible to it), as the Tiltfile arranges.
set -euo pipefail
cd "$(dirname "$0")"

export CAOS_SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}

# Publish the built-ins deep-deps needs (itself + fold) at refs/caos/std.
echo "building caos client + publishing caos/std (fold, deep-deps)..." >&2
nix build .#client -o result-client
./build-builtins.sh fold deep-deps >/dev/null
caos=$PWD/result-client/bin/client

# CAS must live on an xattr-capable fs (caos records each path's hash in
# user.caos.hash); the repo's fs qualifies, /tmp may not.
CAS=$PWD/.caos-dev/test-cas
PKGS=$(mktemp -d)
rm -rf "$CAS"; mkdir -p "$CAS"
export CAOS_CAS_DIR=$CAS
trap 'rm -rf "$CAS" "$PKGS" "$SNAP"' EXIT
SNAP=$(mktemp -d)

# Materialize std locally so we can run the deep-deps built-in by path. (`caos
# run` independently resolves caos/std and threads it in as the run's `std`.)
"$caos" get-hash "$("$caos" resolve refs/caos/std)" "$CAS/std" >/dev/null
IMG="$CAS/std/deep-deps"

fail() { echo "FAIL: $*" >&2; exit 1; }
misses_since() { docker logs --since "$1" caos-server 2>&1 \
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
run
for n in a b c d; do
  diff -r "$SNAP/$n" "$CAS/out/$n" >/dev/null \
    || fail "$n changed after editing an unrelated package"
done
[ -e "$CAS/out/e" ] || fail "e missing from output"
echo "  ok: a,b,c,d byte-identical; e added" >&2

echo "== Phase C2: editing leaf d recomputes everything that reaches d ==" >&2
mkdir -p "$PKGS/x"; : > "$PKGS/x/DEPS"
printf 'x\n' > "$PKGS/d/DEPS"
run
for n in a b c d; do
  diff -r "$SNAP/$n" "$CAS/out/$n" >/dev/null \
    && fail "$n should have changed when d changed"
done
[ -e "$CAS/out/d/DEEP-DEPS/x" ] || fail "d should now depend on x"
echo "  ok: a,b,c,d all recomputed" >&2

echo "== Phase E: bumping caos/std recomputes (std is in the key) ==" >&2
# Re-publish std with an extra entry (hello) — same fold/deep-deps, new std tree.
# `caos run` re-resolves caos/std, so the run is keyed on the new std and misses.
sleep 1; since=$(date +%s)
./build-builtins.sh fold deep-deps hello >/dev/null
run
sleep 1
[ "$(misses_since "$since")" -gt 0 ] || fail "bumping std should have recomputed"
[ -e "$CAS/out/a/DEEP-DEPS/b" ] || fail "output wrong after std bump"
echo "  ok: std bump forced recompute; output still correct" >&2

echo "== Phase D: a dependency cycle is detected (by the server) ==" >&2
# Close a loop: d -> a, so a -> b -> d -> a. The fold recursion re-enters the same
# (fold image, args) and the server's run-cycle detection catches it.
rm -rf "$CAS/pkgs2" "$CAS/cyc"
printf 'a\n' > "$PKGS/d/DEPS"
"$caos" put "$PKGS" "$CAS/pkgs2" >/dev/null
if msg=$("$caos" run "$IMG" "$CAS/cyc" -- --mode=all --packages="$CAS/pkgs2" 2>&1); then
  fail "expected the cyclic graph to fail, but the run succeeded"
fi
echo "$msg" | grep -q "run cycle detected" || fail "no cycle reported; got: $msg"
echo "  ok: run failed with a run-cycle error" >&2

echo "ALL PASS" >&2
