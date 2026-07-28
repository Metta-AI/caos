#!/usr/bin/env bash
#@doc Build the test stack image from the tree: one flake-builder job over
#@doc the workspace, whose #caosImage is a complete caos stack built from
#@doc these sources. Succeeds with that image, which the suite runs once per
#@doc test and anyone can curry a worker1 onto.
#
# THE build worker (design/test-stack-image.md): a workspace tree in (--in,
# run-then or a tool call), the IMAGE out. That is the whole tool — the
# stages, the pinned debian/nix bases, the toolchain bake and the --bins
# handoff are gone, because the flake path already does all of it: the
# builder seeds itself from the tree's #depsImage (so a source edit rebuilds
# only the workspace crates) and memoizes the result in the registry on the
# tree's own hash.
#
# Compiling is still nix's job — it just happens in a worker now, from
# source, rather than on the host with the binaries handed in.
set -euo pipefail

caos get /cas/args/in
[ -e /cas/args/in/flake.nix ] || {
  echo "build: no flake.nix in --in; this tool builds the caos tree" >&2
  exit 1
}

# A tail call, no worker slot held: the flake-builder's own result — the
# image — is this tool's result.
builder=$(caos curry /cas/std/flake-builder --)
caos run-then /cas/args/in -- --run="$builder"
