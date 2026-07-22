#!/bin/bash
# Suite stage 2 (the `then` of the workspace build): check the build, then
# fan out the BASE-IMAGE jobs (phase D1) — the runner and bash worker images
# plus the nix-builder image (stock nixos/nix + caos: phase D2's bake runs
# in it), each assembled by suite-images.sh from a PINNED stock base (the
# images/*-base.ref files in the tree — digests, so the keys are honest) +
# the freshly caos-built binaries, pushed to the caos registry, returned as
# digest refs. Stage 2b takes those refs, bakes the cargo toolchain base,
# and the chain continues to the tests.
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
caos get /cas/args/workspace
caos get /cas/args/workspace/images

spec() { # <name> <base ref blob> <worker source path>
  mkdir -p "/tmp/imgs/$1/files/usr/bin"
  ln -s "$2" "/tmp/imgs/$1/base"
  ln -s /cas/args/result/bin/caos "/tmp/imgs/$1/files/usr/bin/caos"
  ln -s "$3" "/tmp/imgs/$1/files/worker"
}
spec runner /cas/args/workspace/images/debian-base.ref /cas/args/result/bin/worker-runner
spec bash /cas/args/workspace/images/debian-base.ref /cas/args/bash_worker
spec nixbuilder /cas/args/workspace/images/nix-base.ref /cas/args/bash_worker
# The bake must run as root: the builder image's nix store is root-owned.
# Same per-image containment grant testenv carries.
echo "CAOS_WORKER_UID=0" > /tmp/imgs/nixbuilder/env
caos put /tmp/imgs /cas/imgs

fwd=(
  "--build:@=/cas/args/result"
  "--build_ws:@=/cas/args/build_ws"
  "--workspace:@=/cas/args/workspace"
  "--run_nested:@=/cas/args/run_nested"
  "--images_script:@=/cas/args/images_script"
  "--bake_script:@=/cas/args/bake_script"
  "--stage2c:@=/cas/args/stage2c"
  "--stage3:@=/cas/args/stage3"
  "--summarize:@=/cas/args/summarize"
)
[ -e /cas/args/api_key ] && fwd+=("--api_key:@=/cas/args/api_key")
[ -e /cas/args/only ] && fwd+=("--only:@=/cas/args/only")

imgmap=$(caos curry /cas/std/testenv -- "--script:@=/cas/args/images_script")
stage2b=$(caos curry /cas/std/bash -- "--script:@=/cas/args/stage2b" "${fwd[@]}")
caos map-then /cas/imgs -- --map="$imgmap" --then="$stage2b"
