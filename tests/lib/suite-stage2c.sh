#!/bin/bash
# Suite stage 2c (the `then` of the toolchain bake): --result is the deps
# base's digest ref. Stack the cargo worker delta — caos + the /worker
# trampoline, both from the caos-built bins — onto it (the same image job
# that built the runner and bash images) and run-then into stage 3.
set -euo pipefail

caos get /cas/args/build
caos get /cas/args/result
caos get /cas/args/workspace
caos get /cas/args/workspace/tests
caos get /cas/args/workspace/tests/lib
LIB=/cas/args/workspace/tests/lib

mkdir -p /tmp/spec/files/usr/bin
cp /cas/args/result /tmp/spec/base
ln -s /cas/args/build/caos /tmp/spec/files/usr/bin/caos
ln -s /cas/args/build/worker-runner /tmp/spec/files/worker
caos put /tmp/spec /cas/spec

fwd=(
  "--build:@=/cas/args/build"
  "--build_ws:@=/cas/args/build_ws"
  "--workspace:@=/cas/args/workspace"
  "--runner_image:@=/cas/args/runner_image"
  "--bash_image:@=/cas/args/bash_image"
)
[ -e /cas/args/api_key ] && fwd+=("--api_key:@=/cas/args/api_key")
[ -e /cas/args/only ] && fwd+=("--only:@=/cas/args/only")

build_img=$(caos curry /cas/std/testenv -- "--script:@=$LIB/suite-images.sh")
stage3=$(caos curry /cas/std/bash -- "--script:@=$LIB/suite-stage3.sh" "${fwd[@]}")
caos run-then /cas/spec -- --run="$build_img" --then="$stage3"
