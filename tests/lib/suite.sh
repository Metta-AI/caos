#!/bin/bash
# THE test suite, as a caos worker (phase C — design/cargo-workers.md). Runs
# in a bash worker on the outer stack, keyed on everything it takes in: the
# workspace tree, the harness scripts, the image IDs, the API key. A full-
# suite cache hit therefore means literally nothing changed; salt to force.
#
# Stage 1 (this script): run-then the workspace build — std/cargo, the old
# known-good caos building the edited tree — into stage 2, which fans out
# one job per test (map-then) and summarizes. The chain holds no worker slot
# between stages: every step is a continuation the server resolves.
set -euo pipefail

# The build's input is a PRUNED tree — just what cargo reads — so editing a
# test's cli.sh (or a doc) never re-keys the build or anything downstream of
# the bin tree. Symlinks + `caos put` reuse recorded hashes; no bytes move.
caos get /cas/args/workspace
mkdir /tmp/build-ws
for e in Cargo.toml Cargo.lock rust-toolchain.toml crates; do
  [ -e "/cas/args/workspace/$e" ] && ln -s "/cas/args/workspace/$e" "/tmp/build-ws/$e"
done
caos put /tmp/build-ws /cas/build-ws

cargo=$(caos curry /cas/std/cargo -- --cmd=build \
  "--target:@=/cas/args/target" --profile=release)

fwd=(
  "--build_ws:@=/cas/build-ws"
  "--workspace:@=/cas/args/workspace"
  "--stage2:@=/cas/args/stage2"
  "--summarize:@=/cas/args/summarize"
  "--run_nested:@=/cas/args/run_nested"
  "--target:@=/cas/args/target"
  "--runner_image:@=/cas/args/runner_image"
  "--bash_image:@=/cas/args/bash_image"
  "--cargo_image:@=/cas/args/cargo_image"
)
[ -e /cas/args/api_key ] && fwd+=("--api_key:@=/cas/args/api_key")
[ -e /cas/args/only ] && fwd+=("--only:@=/cas/args/only")

stage2=$(caos curry /cas/std/bash -- "--script:@=/cas/args/stage2" "${fwd[@]}")
caos run-then /cas/build-ws -- --run="$cargo" --then="$stage2"
