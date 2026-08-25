#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (tests/lib stages the repo, then runs this).
#
# The literal-tree lints (design/flake-images.md, part 2). The lock lint that
# used to lead here is gone with the duplication it policed: there is ONE
# `flake.lock` at the repo root, DEPped and bound by every flake entry
# (`--lock:@=DEEP-DEPS/flake.lock`), so there are no checked-in copies to
# re-derive and nothing for a lint to keep honest.
#
# What is left are the two lints that check a RULE rather than a copy, each by
# re-deriving the rule instead of restating a result. Fast: no compiles, no
# caos jobs. The workspace arrives in this test's wrapper as $CAOS_PROJECT.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

# The only check in this suite that covers `nix build`. Everything else compiles
# the tree with cargo over the real rust/crates directory, so the flake's `src`
# filter — which hands rustc a FILTERED copy — is exercised by nothing. A file
# it drops is invisible to a green run; that is exactly how the githist/*.sh
# embeds landed. It re-derives the rule rather than restating a result.
echo "== lint-flake-src.sh: every embedded file survives the flake's src filter ==" >&2
bash "$CAOS_PROJECT/lint-flake-src.sh" "$CAOS_PROJECT" \
  || fail "a file the crates compile from is dropped by the flake's src filter"

echo "== lint-bake-anchor.sh: every std tool's crates.io deps are anchored ==" >&2
bash "$CAOS_PROJECT/lint-bake-anchor.sh" \
  || fail "a std tool's crates.io dep is missing from bake-anchor (see above)"

echo "std-lint: ALL PASS" >&2
