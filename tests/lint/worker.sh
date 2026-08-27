#!/bin/bash
# tests/lint — a WORKER test: no client, no repo.
#
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
# `/cas/args/in/DEEP-DEPS` IS THE TREE TO CHECK. `in` is this test's own
# deepened tree — dev/run-test runs a test by `run-then`ning it — so each
# dependency is mounted under its own repo name and both lints take that one
# directory as their root. Reaching it through `in` rather than binding it is
# deliberate: `std` is evaluable, and naming it with `:@=` would build every
# std entry to read three Cargo.toml files.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

caos get -r /cas/args/in || fail "materializing this test's dependencies"
caos get /cas/args/src-lint || fail "reading lint-flake-src.sh"
caos get /cas/args/bake-lint || fail "reading lint-bake-anchor.sh"
root=/cas/args/in/DEEP-DEPS
# BOTH LINTS PASS VACUOUSLY ON AN EMPTY TREE — one walks `crates/**/*.rs`, the
# other `std/*/Cargo.toml`, and neither has anything to say about a glob that
# matched nothing. So the mounts are asserted here, where a wrong `DEPS` is a
# loud failure rather than a green run that checked nothing.
[ -d "$root/rust/crates" ] || fail "no rust/crates under $root — DEPS did not mount"
[ -e "$root/flake.nix" ]   || fail "no flake.nix under $root — DEPS did not mount"
tomls=("$root"/std/*/Cargo.toml)
[ -e "${tomls[0]}" ] || fail "no std/*/Cargo.toml under $root — DEPS did not mount"
echo "checking ${#tomls[@]} std Cargo.toml file(s) under $root" >&2

# The only check in this suite that covers `nix build`. Everything else compiles
# the tree with cargo over the real rust/crates directory, so the flake's `src`
# filter — which hands rustc a FILTERED copy — is exercised by nothing. A file
# it drops is invisible to a green run; that is exactly how the githist/*.sh
# embeds landed.
echo "== lint-flake-src.sh: every embedded file survives the flake's src filter ==" >&2
bash /cas/args/src-lint "$root" \
  || fail "a file the crates compile from is dropped by the flake's src filter"

# Same shape: a tree to check, as an argument.
echo "== lint-bake-anchor.sh: every std tool's crates.io deps are anchored ==" >&2
bash /cas/args/bake-lint "$root" \
  || fail "a std tool's crates.io dep is missing from bake-anchor (see above)"

printf 'lint: ALL PASS (%s std Cargo.toml files checked)\n' "${#tomls[@]}" > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
