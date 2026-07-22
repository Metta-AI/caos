#!/usr/bin/env bash
#@doc Build the workspace. Compiles per-crate — unchanged crates are cache
#@doc hits, and a compile error in an edited crate surfaces in seconds —
#@doc then links the binaries. Succeeds with the bin tree; fails with the
#@doc compiler diagnostics.
#
# THE build worker: a workspace tree in (--in, run-then or a tool call), the
# content-stable bin tree out. Runs in a bash worker. The suite's stage 1
# runs this same script over the same pruned tree, so an agent's `build`
# call and the test suite share every cargo job in the cache.
#
# Prunes to what cargo reads first (idempotent on an already-pruned tree),
# so non-Rust edits never re-key the build; builds with --mode=all (the
# per-crate decomposition) so an edit recompiles the touched crates and
# their dependents; strips the result to bin/ (strip-bins.sh, curried as
# --strip: cargo's stderr carries volatile timings, and nothing downstream
# may key on it).
set -euo pipefail

caos get /cas/args/in
mkdir /tmp/bw
for e in Cargo.toml Cargo.lock rust-toolchain.toml crates; do
  [ -e "/cas/args/in/$e" ] && ln -s "/cas/args/in/$e" "/tmp/bw/$e"
done
caos put /tmp/bw /cas/bw

cargo=$(caos curry /cas/std/cargo -- --cmd=build --mode=all \
  "--target=$(uname -m)-unknown-linux-musl" --profile=release)
strip=$(caos curry /cas/std/bash -- "--script:@=/cas/args/strip")
caos run-then /cas/bw -- --run="$cargo" --then="$strip"
