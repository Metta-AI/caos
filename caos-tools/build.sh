#!/usr/bin/env bash
#@doc Build the tree's worker images from the nix-built binaries: the
#@doc runner/bash images, the toolchain deps bake, and the cargo worker
#@doc image. Compiling is nix's job — the deploy (caosd up) publishes the
#@doc binaries as refs/caos/bins and run-tool passes the hash as --bins.
#@doc Succeeds with the artifact tree {report, bin/, images/}.
#
# THE build worker: a workspace tree in (--in, run-then or a tool call)
# plus the deploy's published bin tree (--bins, a hash), the ARTIFACT TREE
# out — {report, bin/<name>, images/{runner,bash,cargo}} (image refs as
# registry-digest blobs). Runs in a bash worker; every stage script comes
# from the tree itself (caos-tools/lib/), so the tool is self-contained
# and a host runner, an agent invocation, and the test suite all fire it
# the same way, sharing every job in the cache.
#
# The chain (each link a run-then continuation, no worker slot held):
#   1. (this script) materialize the bin tree, fan out the base-image
#      jobs (runner, bash, nix-builder) from pinned stock bases + those
#      binaries (lib/image-build.sh);
#   2. lib/build-stage2b.sh: bake the toolchain deps base (nix, in the
#      builder; registry-memoized by the bake tree's content hash);
#   3. lib/build-stage2c.sh: stack the cargo worker image onto it;
#   4. lib/build-final.sh: assemble the artifact tree.
set -euo pipefail

if [ ! -e /cas/args/bins ]; then
  echo "build: no --bins. The deploy publishes the nix-built binaries as" >&2
  echo "build: refs/caos/bins (caosd up); run-tool passes the hash along." >&2
  exit 1
fi
# --bins is the published bin TREE's hash (canonical runtime names, see
# build-builtins.sh): materialize it — recorded hashes, no bytes move.
caos get /cas/args/bins
caos get-hash "$(cat /cas/args/bins)" /cas/bin
caos get /cas/args/in
caos get /cas/args/in/images
caos get /cas/args/in/caos-tools
caos get /cas/args/in/caos-tools/lib
LIB=/cas/args/in/caos-tools/lib

# The base-image jobs. An image job's key is exactly (builder script, base
# ref, file contents) — unchanged binaries mean an instant hit and no
# build.
spec() { # <name> <base ref blob> <worker source path>
  mkdir -p "/tmp/imgs/$1/files/usr/bin"
  ln -s "$2" "/tmp/imgs/$1/base"
  ln -s /cas/bin/caos "/tmp/imgs/$1/files/usr/bin/caos"
  ln -s "$3" "/tmp/imgs/$1/files/worker"
}
spec runner /cas/args/in/images/debian-base.ref /cas/bin/worker-runner
spec bash /cas/args/in/images/debian-base.ref /cas/args/in/images/bash-worker.sh
spec nixbuilder /cas/args/in/images/nix-base.ref /cas/args/in/images/bash-worker.sh

# runner and bash are part of the test stack but nixbuilder is part of the
# host stack and is used to build other parts of the test stack. As such,
# it should have the host caos binary, not the test caos binary. This has
# the fortunate side effect of making the cache key for nixbuilder stable
# across changes to the test stack's caos, which is desirable because if we
# rebuild nixbuilder we also have to rebuild anything that it builds,
# including the toolchain, which is very slow.
#
# The tested caos is layered onto the toolchain image separately
# (build-stage2c), so nothing host leaks into the test world. (runner/bash
# above DO carry the tested caos — they're the nested stack's own images.)
rm /tmp/imgs/nixbuilder/files/usr/bin/caos
cp /bin/caos /tmp/imgs/nixbuilder/files/usr/bin/caos

# The bake must run as root: the builder image's nix store is root-owned.
# Same per-image containment grant testenv carries.
echo "CAOS_WORKER_UID=0" > /tmp/imgs/nixbuilder/env
caos put /tmp/imgs /cas/imgs

imgmap=$(caos curry /cas/std/testenv -- "--worker1:@=$LIB/image-build.sh")
stage2b=$(caos curry /cas/std/bash -- "--worker1:@=$LIB/build-stage2b.sh" \
  "--workspace:@=/cas/args/in" "--bins:@=/cas/bin")
caos map-then /cas/imgs -- --map="$imgmap" --then="$stage2b"
