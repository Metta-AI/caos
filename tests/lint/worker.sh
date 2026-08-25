#!/usr/bin/env bash
# The literal-tree lints (design/flake-images.md, part 2), run over this test's
# own declared dependencies rather than over a workspace handed to it.
#
# The lock lint that used to lead here is gone with the duplication it policed:
# there is ONE `flake.lock` at the repo root now, DEPped and bound by every
# flake entry, so there are no checked-in copies to re-derive.
#
# What is left are two lints that check a RULE by re-deriving it, rather than
# restating a result. Fast: no compiles, no caos jobs, no client.
#
# `DEEP-DEPS` IS THE TREE TO CHECK. Each dependency is mounted under its own repo
# name, so `rust/`, `std/` and `flake.nix` sit beside each
# other exactly as they do in the repo, and both lints take that directory as
# their root. The scripts themselves live HERE, in the test that is their only
# caller, and arrive at ./test with the rest of this directory.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

# The only check in this suite that covers `nix build`. Everything else compiles
# the tree with cargo over the real rust/crates directory, so the flake's `src`
# filter — which hands rustc a FILTERED copy — is exercised by nothing. A file
# it drops is invisible to a green run; that is exactly how the githist/*.sh
# embeds landed.
echo "== lint-flake-src.sh: every embedded file survives the flake's src filter ==" >&2
bash test/lint-flake-src.sh DEEP-DEPS \
  || fail "a file the crates compile from is dropped by the flake's src filter"

# Same shape: a tree to check, as an argument.
echo "== lint-bake-anchor.sh: every std tool's crates.io deps are anchored ==" >&2
bash test/lint-bake-anchor.sh DEEP-DEPS \
  || fail "a std tool's crates.io dep is missing from bake-anchor (see above)"

echo "lint: ALL PASS" >&2
