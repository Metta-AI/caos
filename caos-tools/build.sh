#!/usr/bin/env bash
#@doc Build the workspace. Compiles per-crate — unchanged crates are cache
#@doc hits, and a compile error in an edited crate surfaces in seconds —
#@doc then links the binaries. Succeeds with the bin tree; fails with the
#@doc compiler diagnostics.
#
# THE build worker: the workspace tree in (--in), the content-stable bin
# tree out. Runs in a bash worker; helpers come from the tree itself
# (caos-tools/lib/), so the tool is self-contained — a host runner, an
# agent invocation, and the suite's stage 1 all fire it the same way, and
# every cargo job lands on shared keys.
#
# Prunes to what cargo reads first, so the cargo jobs never re-key on
# non-Rust edits; builds with --mode=all (the per-crate decomposition) so
# an edit recompiles the touched crates and their dependents; strips the
# result to bin/ (lib/strip-bins.sh: cargo's stderr carries volatile
# timings, and nothing downstream may key on it).
set -euo pipefail

caos get /cas/args/in
caos get /cas/args/in/caos-tools
caos get /cas/args/in/caos-tools/lib

mkdir /tmp/bw
for e in Cargo.toml Cargo.lock rust-toolchain.toml crates; do
  [ -e "/cas/args/in/$e" ] && ln -s "/cas/args/in/$e" "/tmp/bw/$e"
done
caos put /tmp/bw /cas/bw

cargo=$(caos curry /cas/std/cargo -- --cmd=build --mode=all \
  "--target=$(uname -m)-unknown-linux-musl" --profile=release)
strip=$(caos curry /cas/std/bash -- \
  "--script:@=/cas/args/in/caos-tools/lib/strip-bins.sh")
caos run-then /cas/bw -- --run="$cargo" --then="$strip"
