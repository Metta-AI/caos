#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (tests/lib stages the repo, then runs this).
#
# The literal-tree lints (design/flake-images.md, part 2): std's checked-in
# redundancies must match what std/refresh.sh regenerates from their sources
# of truth — each std flake.lock re-derives byte-identically from the root
# flake.lock.
# The check IS the generator run in --check mode (one script, one code
# path), so the lint cannot drift from what a refresh would write. The
# workspace arrives in this test's wrapper, staged as $CAOS_PROJECT
# (caos-tools/test.sh, the `fanout` stage). Fast: no compiles, no caos jobs.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== std/refresh.sh --check: every checked-in std copy re-derives ==" >&2
bash "$CAOS_PROJECT/std/refresh.sh" --check || fail "checked-in std copies are stale"

# The other literal-tree lint, and the only check in this suite that covers
# `nix build`. Everything else compiles the tree with cargo over the real
# crates/ directory, so the flake's `src` filter — which hands rustc a FILTERED
# copy — is exercised by nothing. A file it drops is invisible to a green run;
# that is exactly how the githist/*.sh embeds landed. Same shape as the lint
# above: it re-derives the rule rather than restating a result.
echo "== lint-flake-src.sh: every embedded file survives the flake's src filter ==" >&2
bash "$CAOS_PROJECT/lint-flake-src.sh" "$CAOS_PROJECT" \
  || fail "a file the crates compile from is dropped by the flake's src filter"

echo "== lint-bake-anchor.sh: every std tool's crates.io deps are anchored ==" >&2
bash "$CAOS_PROJECT/lint-bake-anchor.sh" \
  || fail "a std tool's crates.io dep is missing from bake-anchor (see above)"

echo "std-lint: ALL PASS" >&2
