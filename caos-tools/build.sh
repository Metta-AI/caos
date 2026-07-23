#!/usr/bin/env bash
#@doc Build EVERYTHING the tree defines: compile the workspace per-crate
#@doc (unchanged crates are cache hits; a compile error surfaces in seconds),
#@doc link the binaries, build the worker images, and produce the toolchain
#@doc image. Succeeds with the artifact tree {report, bin/, images/}; fails
#@doc with the diagnostics of whichever stage broke.
#
# THE build worker: a workspace tree in (--in, run-then or a tool call), the
# ARTIFACT TREE out — {report, bin/<name>, images/{runner,bash,cargo}} (image
# refs as registry-digest blobs). Runs in a bash worker; every stage script
# comes from the tree itself (caos-tools/lib/), so the tool is self-contained
# and a host runner, an agent invocation, and the test suite all fire it the
# same way, sharing every job in the cache.
#
# The chain (each link a run-then continuation, no worker slot held):
#   1. (this script) prune to what cargo reads -> the per-crate workspace
#      build (std/cargo --mode=all);
#   2. lib/build-stage2.sh: check the compile, fan out the base-image jobs
#      (runner, bash, nix-builder) from pinned stock bases + the fresh bins;
#   3. lib/build-stage2b.sh: bake the toolchain deps base (nix, in the
#      builder; registry-memoized by the bake tree's content hash);
#   4. lib/build-stage2c.sh: stack the cargo worker image onto it;
#   5. lib/build-final.sh: assemble the artifact tree.
set -euo pipefail

caos get /cas/args/in
caos get /cas/args/in/caos-tools
caos get /cas/args/in/caos-tools/lib
LIB=/cas/args/in/caos-tools/lib

# The pruned tree — just what cargo reads — keys the compile, so non-Rust
# edits never re-key it or anything downstream of the bin tree. Symlinks +
# `caos put` reuse recorded hashes; no bytes move.
mkdir /tmp/bw
for e in Cargo.toml Cargo.lock rust-toolchain.toml crates; do
  [ -e "/cas/args/in/$e" ] && ln -s "/cas/args/in/$e" "/tmp/bw/$e"
done
caos put /tmp/bw /cas/bw

# Static musl (runs on any base) at the default dev profile — the one profile
# the deps bake carries, so only workspace crates recompile per edit. dev keeps
# debug_assert!/overflow checks live in the produced bins and test binaries.
cargo=$(caos curry /cas/std/cargo -- --cmd=build --mode=all \
  "--target=$(uname -m)-unknown-linux-musl")
stage2=$(caos curry /cas/std/bash -- "--script:@=$LIB/build-stage2.sh" \
  "--workspace:@=/cas/args/in")
caos run-then /cas/bw -- --run="$cargo" --then="$stage2"
