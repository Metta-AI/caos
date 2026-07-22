#!/bin/bash
# Suite stage 2 (the `then` of the workspace build): check the build, then
# fan out the IMAGE-BUILD jobs (phase D1) — the runner and bash worker
# images, assembled from the stock debian base + the freshly caos-built
# binaries by suite-images.sh, pushed to the caos registry, returned as
# digest refs. Stage 3 takes those refs and fans out the tests.
#
# The specs are built with symlinks + `caos put` (recorded-hash reuse), so
# an image job's key is exactly (builder script, base ref, file contents):
# unchanged binaries mean an instant hit and no build.
set -euo pipefail

caos get /cas/args/result
caos get /cas/args/result/exit
if [ "$(cat /cas/args/result/exit)" != 0 ]; then
  caos get /cas/args/result/stderr || true
  tail -60 /cas/args/result/stderr >&2 || true
  echo "SUITE: workspace build failed" >&2
  exit 1
fi
caos get /cas/args/result/bin

BASE=docker.io/library/debian:stable-slim
spec() { # <name> <worker source path>
  mkdir -p "/tmp/imgs/$1/files/usr/bin"
  printf '%s' "$BASE" > "/tmp/imgs/$1/base"
  ln -s /cas/args/result/bin/caos "/tmp/imgs/$1/files/usr/bin/caos"
  ln -s "$2" "/tmp/imgs/$1/files/worker"
}
spec runner /cas/args/result/bin/worker-runner
spec bash /cas/args/bash_worker
caos put /tmp/imgs /cas/imgs

fwd=(
  "--build:@=/cas/args/result"
  "--build_ws:@=/cas/args/build_ws"
  "--workspace:@=/cas/args/workspace"
  "--run_nested:@=/cas/args/run_nested"
  "--summarize:@=/cas/args/summarize"
  "--cargo_image:@=/cas/args/cargo_image"
)
[ -e /cas/args/api_key ] && fwd+=("--api_key:@=/cas/args/api_key")
[ -e /cas/args/only ] && fwd+=("--only:@=/cas/args/only")

imgmap=$(caos curry /cas/std/testenv -- "--script:@=/cas/args/images_script")
stage3=$(caos curry /cas/std/bash -- "--script:@=/cas/args/stage3" "${fwd[@]}")
caos map-then /cas/imgs -- --map="$imgmap" --then="$stage3"
