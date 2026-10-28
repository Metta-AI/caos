#!/bin/bash
# tests/flake-update — a WORKER test: no client, no repo.
#
# Runs the tool twice over ./flake, a fixture whose one input is
# github:nix-systems/default (a tiny repo with no inputs of its own).
#
# Covers: locking from flake.nix alone (nix fetches the tip, and the lock names
# a commit and a narHash); pinning one input to a commit through its `original`
# (the lock then carries exactly that commit, the narHash nix computes for it is
# the one every flake.lock pinning that commit carries, and the original is
# still what flake.nix says); and the result being the lock alone, a blob.
#
# STAGES: a run cannot be waited on, so each assertion is the `then` of the run
# it is about.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
# EVERYTHING A LATER STAGE READS IS FORWARDED: `/cas/args/base` is the reserved
# base entry, not this job's ArgTree.
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --update:@=/cas/args/update --flake:@=/cas/args/flake "$@"; }

# The lock entry every flake pinning this commit of nix-systems/default carries.
PIN=da67096a3b9bf56a91d16901293e51ba5b49a27e
PIN_NAR=sha256-b+uqzj+Wa6xgMS9aNbX4I+sXeb5biPDi39VgvSFqFvU=

case "$stage" in

start)
  echo "== lock from flake.nix alone ==" >&2
  caos run-then /cas/args/flake --run:hash="$(caos curry --base:@=/cas/args/update)" \
    --then:hash="$(next locked)"
  ;;

locked)
  R=/cas/args/result
  caos get "$R" || fail "reading the result"
  [ -f "$R" ] || fail "the result is not a blob"
  jq -e '.nodes.systems.locked | .type == "github" and (.rev | length) == 40 and (.narHash | startswith("sha256-"))' "$R" >/dev/null \
    || fail "the lock does not pin systems to a github commit: $(cat "$R")"
  echo "  ok: systems locked at $(jq -r .nodes.systems.locked.rev "$R")" >&2

  echo "== pin one input to a commit ==" >&2
  caos get /cas/args/flake
  caos get /cas/args/flake/flake.nix
  mkdir /tmp/flake2
  cp -L /cas/args/flake/flake.nix /tmp/flake2/
  cp -L "$R" /tmp/flake2/flake.lock
  caos put /tmp/flake2 /cas/flake2
  caos run-then /cas/flake2 \
    --run:hash="$(caos curry --base:@=/cas/args/update --input=systems --rev="$PIN")" \
    --then:hash="$(next pinned)"
  ;;

pinned)
  R=/cas/args/result
  caos get "$R" || fail "reading the result"
  got=$(jq -r .nodes.systems.locked.rev "$R")
  [ "$got" = "$PIN" ] || fail "systems pinned to $got, not $PIN"
  nar=$(jq -r .nodes.systems.locked.narHash "$R")
  [ "$nar" = "$PIN_NAR" ] || fail "narHash for $PIN is $nar, expected $PIN_NAR"
  jq -e '.nodes.systems.original == {owner: "nix-systems", repo: "default", type: "github"}' "$R" >/dev/null \
    || fail "original changed: $(jq -c .nodes.systems.original "$R")"
  echo "  ok: pinned, narHash agrees, original kept" >&2

  printf 'flake-update: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
