#!/bin/bash
# Build stage 2c (the `then` of the toolchain bake): --result is the deps
# base's digest ref. Stack the cargo worker delta — caos + the /worker
# trampoline, both from the fresh bins — onto it (the same image job that
# built the runner and bash images) and run-then into the final assembly.
set -euo pipefail

caos get /cas/args/bin
caos get /cas/args/result
caos get /cas/args/workspace
caos get /cas/args/workspace/caos-tools
caos get /cas/args/workspace/caos-tools/lib
LIB=/cas/args/workspace/caos-tools/lib

mkdir -p /tmp/spec/files/usr/bin
cp /cas/args/result /tmp/spec/base
ln -s /cas/args/bin/caos /tmp/spec/files/usr/bin/caos
ln -s /cas/args/bin/worker-runner /tmp/spec/files/worker
caos put /tmp/spec /cas/spec

build_img=$(caos curry /cas/std/testenv -- "--script:@=$LIB/image-build.sh")
final=$(caos curry /cas/std/bash -- "--script:@=$LIB/build-final.sh" \
  "--bin:@=/cas/args/bin" \
  "--runner_image:@=/cas/args/runner_image" \
  "--bash_image:@=/cas/args/bash_image")
caos run-then /cas/spec -- --run="$build_img" --then="$final"
