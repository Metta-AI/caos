#!/bin/bash
# Suite stage 2 (the `then` of the build worker): --result IS the bin tree
# (the build worker fails loudly on a broken build, so reaching here means
# it's whole). Fan out the BASE-IMAGE jobs — the runner and bash worker
# images plus the nix-builder image (stock nixos/nix + caos: the toolchain
# bake runs in it), each assembled by suite-images.sh from a PINNED stock
# base (the images/*-base.ref files in the tree — digests, so the keys are
# honest) + the freshly caos-built binaries, pushed to the caos registry,
# returned as digest refs.
#
# The specs are built with symlinks + `caos put` (recorded-hash reuse), so
# an image job's key is exactly (builder script, base ref, file contents):
# unchanged binaries mean an instant hit and no build.
set -euo pipefail

caos get /cas/args/result
caos get /cas/args/workspace
caos get /cas/args/workspace/tests
caos get /cas/args/workspace/tests/lib
caos get /cas/args/workspace/images
LIB=/cas/args/workspace/tests/lib

spec() { # <name> <base ref blob> <worker source path>
  mkdir -p "/tmp/imgs/$1/files/usr/bin"
  ln -s "$2" "/tmp/imgs/$1/base"
  ln -s /cas/args/result/caos "/tmp/imgs/$1/files/usr/bin/caos"
  ln -s "$3" "/tmp/imgs/$1/files/worker"
}
spec runner /cas/args/workspace/images/debian-base.ref /cas/args/result/worker-runner
spec bash /cas/args/workspace/images/debian-base.ref /cas/args/workspace/images/bash-worker.sh
spec nixbuilder /cas/args/workspace/images/nix-base.ref /cas/args/workspace/images/bash-worker.sh
# The bake must run as root: the builder image's nix store is root-owned.
# Same per-image containment grant testenv carries.
echo "CAOS_WORKER_UID=0" > /tmp/imgs/nixbuilder/env
caos put /tmp/imgs /cas/imgs

fwd=(
  "--build:@=/cas/args/result"
  "--build_ws:@=/cas/args/build_ws"
  "--workspace:@=/cas/args/workspace"
)
[ -e /cas/args/api_key ] && fwd+=("--api_key:@=/cas/args/api_key")
[ -e /cas/args/only ] && fwd+=("--only:@=/cas/args/only")

imgmap=$(caos curry /cas/std/testenv -- "--script:@=$LIB/suite-images.sh")
stage2b=$(caos curry /cas/std/bash -- "--script:@=$LIB/suite-stage2b.sh" "${fwd[@]}")
caos map-then /cas/imgs -- --map="$imgmap" --then="$stage2b"
