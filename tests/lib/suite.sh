#!/bin/bash
# THE test suite, as a caos worker (phase C — design/cargo-workers.md). Runs
# in a bash worker on the outer stack, keyed on everything it takes in: the
# workspace tree, the harness scripts, the cargo image ID, the API key. A
# full-suite cache hit therefore means literally nothing changed; salt to
# force.
#
# The chain, no worker slot held between stages (continuations all the way):
#   1. (this script) run-then the workspace build — std/cargo, the old
#      known-good caos building the edited tree — into stage 2;
#   2. stage 2: fan out the IMAGE-BUILD jobs (runner + bash worker images
#      from the stock debian base + the caos-built binaries, pushed to the
#      caos registry — phase D1);
#   3. stage 3: fan out one job per tests/<name>/cli.sh;
#   4. summarize: the report + every test's complete record.
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
  "--stage3:@=/cas/args/stage3"
  "--images_script:@=/cas/args/images_script"
  "--summarize:@=/cas/args/summarize"
  "--run_nested:@=/cas/args/run_nested"
  "--bash_worker:@=/cas/args/bash_worker"
  "--cargo_image:@=/cas/args/cargo_image"
)
[ -e /cas/args/api_key ] && fwd+=("--api_key:@=/cas/args/api_key")
[ -e /cas/args/only ] && fwd+=("--only:@=/cas/args/only")

stage2=$(caos curry /cas/std/bash -- "--script:@=/cas/args/stage2" "${fwd[@]}")
caos run-then /cas/build-ws -- --run="$cargo" --then="$stage2"
