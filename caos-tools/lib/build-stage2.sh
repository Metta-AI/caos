#!/bin/bash
# Build stage 2 (the `then` of the workspace compile): fail loudly on a
# broken compile, else fan out the BASE-IMAGE jobs — the runner and bash
# worker images plus the nix-builder (the toolchain bake runs in it), each
# assembled by lib/image-build.sh from a PINNED stock base (images/*-base.ref
# — digests, so the keys are honest) + the freshly built binaries, pushed to
# the caos registry, returned as digest refs.
#
# Specs are built with symlinks + `caos put` (recorded-hash reuse): an image
# job's key is exactly (builder script, base ref, file contents) — unchanged
# binaries mean an instant hit and no build.
set -euo pipefail

caos get /cas/args/result
caos get /cas/args/result/exit
# A compile failure is a VALUE, not a job error: the artifact tree becomes
# {report} carrying the diagnostics, so `run-tool build` prints WHY it failed
# (and the suite's stage 3 sees the missing bin/ and reports the build
# failure instead of fanning out tests). Same "failures are values" contract
# as the test verdicts.
fail_report() { # <headline>  (diagnostics on /cas/args/result/{stdout,stderr})
  caos get /cas/args/result/stdout 2>/dev/null || true
  caos get /cas/args/result/stderr 2>/dev/null || true
  mkdir -p /tmp/art
  {
    echo "BUILD FAILED: $1"
    echo
    for s in stdout stderr; do
      if [ -s "/cas/args/result/$s" ]; then
        echo "── $s ──"
        tail -80 "/cas/args/result/$s"
      fi
    done
  } > /tmp/art/report
  caos put /tmp/art /cas/out
  exit 0
}
if [ "$(cat /cas/args/result/exit)" != 0 ]; then
  fail_report "workspace compile failed"
fi
if [ ! -e /cas/args/result/bin ]; then
  fail_report "compile succeeded but staged no bin tree"
fi
caos get /cas/args/result/bin
caos get /cas/args/workspace
caos get /cas/args/workspace/images
caos get /cas/args/workspace/caos-tools
caos get /cas/args/workspace/caos-tools/lib
LIB=/cas/args/workspace/caos-tools/lib

spec() { # <name> <base ref blob> <worker source path>
  mkdir -p "/tmp/imgs/$1/files/usr/bin"
  ln -s "$2" "/tmp/imgs/$1/base"
  ln -s /cas/args/result/bin/caos "/tmp/imgs/$1/files/usr/bin/caos"
  ln -s "$3" "/tmp/imgs/$1/files/worker"
}
spec runner /cas/args/workspace/images/debian-base.ref /cas/args/result/bin/worker-runner
spec bash /cas/args/workspace/images/debian-base.ref /cas/args/workspace/images/bash-worker.sh
spec nixbuilder /cas/args/workspace/images/nix-base.ref /cas/args/workspace/images/bash-worker.sh

# runner and bash are part of the test stack but nixbuilder is part of the host
# stack and is used to build other parts of the test stack. As such, it should
# have the host caos binary, not the test caos binary. This has the fortunate
# side effect of making the cache key for nixbuilder stable across changes to
# the test stack's caos, which is desirable becasue if we rebuild nixbuilder we
# also have to rebuild anything that it builds, including the toolchain, which
# is very slow
#
# The tested caos is layered onto the toolchain image
# separately (build-stage2c), so nothing host leaks into the test world.
# (runner/bash above DO carry the tested caos — they're the nested stack's
# own images.)
rm /tmp/imgs/nixbuilder/files/usr/bin/caos
cp /bin/caos /tmp/imgs/nixbuilder/files/usr/bin/caos

# The bake must run as root: the builder image's nix store is root-owned.
# Same per-image containment grant testenv carries.
echo "CAOS_WORKER_UID=0" > /tmp/imgs/nixbuilder/env
caos put /tmp/imgs /cas/imgs

imgmap=$(caos curry /cas/std/testenv -- "--script:@=$LIB/image-build.sh")
stage2b=$(caos curry /cas/std/bash -- "--script:@=$LIB/build-stage2b.sh" \
  "--workspace:@=/cas/args/workspace" "--bin:@=/cas/args/result/bin")
caos map-then /cas/imgs -- --map="$imgmap" --then="$stage2b"
