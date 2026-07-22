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

cargo=$(caos curry /cas/std/cargo -- --cmd=build \
  "--target:@=/cas/args/target" --profile=release)

fwd=(
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

stage2=$(caos curry /cas/std/bash -- "--script:@=/cas/args/stage2" "${fwd[@]}")
caos run-then /cas/args/workspace -- --run="$cargo" --then="$stage2"
