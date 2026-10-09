#!/usr/bin/env bash
# tests/nix-host — a WORKER test: no client, no repo, no stack.
#
# Nothing else in this tree compiles prod/caosd. `nix flake check` looks at
# nixosConfigurations, but only shallowly: it validates the shape and PASSES on
# a host that cannot be built at all. So a renamed option, a nixpkgs bump that
# drops an attribute, or a malformed unit would surface at
# `switch-to-configuration` time — on a real machine, mid-deploy. This is the
# gate that catches it here instead.
#
# WHY IT IS AFFORDABLE. The image is dev/test-stack, whose volume grant puts
# the HOST's nix store at /nix (SPEC, "Mounts a persistent volume at
# /mounted-nix ... and then mounts /mounted-nix at /nix"). That is what keeps
# this from being a from-scratch NixOS build: nixpkgs, the rust toolchain and
# the whole dependency closure are already there. std/caos-build leans on the
# same property.
#
# The caos derivations themselves may still rebuild — the tree arrives from CAS
# at a different path than the one the host built from, and nix keys a `path:`
# flake on the source it is given. Measured on a warm store: 14 derivations,
# dominated by caos-cli. Worth knowing before assuming this test is free.
#
# ONE STAGE. Every claim is about what nix says about a tree that is already in
# hand, so nothing here has to delegate a continuation.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

# The placeholder address. TEST-NET-3 (RFC 5737), reserved for documentation,
# so it cannot be mistaken for a real host and is obviously not a default that
# leaked out of the role. caos.advertiseAddress is per-machine and has no
# default (prod/README.md), so the host cannot be built without one.
ADDR=203.0.113.1

# The deepened tree IS the flake root: DEEP-DEPS holds flake.nix beside prod/,
# rust/, std/ and stack/, which is the layout the flake's own `./prod`, `./rust`
# and `./std` references expect.
caos get -r /cas/args/in || fail "materializing this test's tree"
root=/cas/args/in/DEEP-DEPS
[ -f "$root/flake.nix" ] || fail "no flake.nix at $root — DEPS layout changed"
[ -d "$root/prod/caosd" ] || fail "no prod/caosd at $root — DEPS layout changed"

# `path:`, like std/caos-build: take the directory as it stands rather than
# looking for a git input in it. The tree arrives from CAS and is not a repo.
FLAKE="path:$root"

# --impure is required twice over: `builtins.getFlake` on an unlocked ref needs
# it, and so does reading a flake from a path that has no lock of its own here.
nix_expr() {
  printf '((builtins.getFlake "%s").nixosConfigurations.caosd-prod.extendModules {' "$FLAKE"
  printf ' modules = [ { caos.advertiseAddress = "%s"; } ]; }).config.system.build.toplevel' "$ADDR"
}

echo "== the caosd-prod host builds, given a per-machine address ==" >&2
# stderr to a FILE, not into $out: --print-out-paths writes the path to stdout
# while nix narrates on stderr, and folding them together made $out start with
# "these N derivations will be built" — a case-match that could never match.
out=$(nix build --impure --no-link --print-out-paths --expr "$(nix_expr)" 2>/tmp/nix-build.err) \
  || fail "the host config does not build: $(tail -20 /tmp/nix-build.err)"
case "$out" in
  /nix/store/*-nixos-system-caosd-prod-*) ;;
  *) fail "built something that is not the caosd-prod system: $out" ;;
esac
echo "  ok: $out" >&2

echo "== the unit that runs the stack is wired to start on boot ==" >&2
# The point of the role is a machine that comes back by itself, so the two
# properties worth pinning are that caosd is WANTED at boot and that it is the
# flake's own caosd, not something that happened to be on PATH.
unit=$out/etc/systemd/system/caosd.service
[ -f "$unit" ] || fail "no caosd.service in the built system"
[ -e "$out/etc/systemd/system/multi-user.target.wants/caosd.service" ] \
  || fail "caosd.service is not wanted by multi-user.target — it would not start on boot"
grep -q '^ExecStart=/nix/store/.*/bin/caosd up --iroh' "$unit" \
  || fail "caosd.service does not start caosd from the store: $(grep ExecStart "$unit")"
echo "  ok: enabled at boot, ExecStart is a store path" >&2

echo "== the per-machine address reaches the unit, not a default ==" >&2
# CAOS_IROH_ADVERTISE is the whole reason the deploy driver exists: EC2 NATs the
# public address, so a host that advertises a private one makes every client
# fall back to an iroh relay. extendModules is how the driver supplies it, and
# this asserts that path actually lands in the unit.
grep -q "CAOS_IROH_ADVERTISE=$ADDR:11204" "$unit" \
  || fail "the supplied address did not reach the unit: $(grep IROH "$unit" || echo none)"
echo "  ok: CAOS_IROH_ADVERTISE=$ADDR:11204" >&2

echo "== without an address the build FAILS, naming the option ==" >&2
# The contract prod/README.md documents: a bare `nixos-rebuild switch --flake
# ...#caosd-prod` must not quietly produce a machine that advertises a private
# address. It must fail, and it must say which option is missing — an
# unexplained evaluation error would send the next person looking in the wrong
# place. This is the claim that keeps the driver necessary rather than optional.
bare="(builtins.getFlake \"$FLAKE\").nixosConfigurations.caosd-prod.config.system.build.toplevel"
if err=$(nix eval --impure --raw --expr "$bare" 2>&1); then
  fail "the host built with no advertiseAddress — the driver contract is broken: $err"
fi
printf '%s\n' "$err" | grep -q "caos.advertiseAddress" \
  || fail "it failed, but not on the named option: $err"
echo "  ok: refused, naming caos.advertiseAddress" >&2

printf 'nix-host: ALL PASS\n' > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
