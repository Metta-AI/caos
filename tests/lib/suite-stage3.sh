#!/bin/bash
# Suite stage 3 (the `then` of the build tool): --result is THE TEST STACK
# IMAGE (design/test-stack-image.md). Warm the std memos in ONE stack, then
# hand off to stage 4, which selects the tests and fans out.
#
# Why the warm-up is its own stage. Every test publishes its own std, which is
# a registry hit once anything has built it — but nineteen stacks starting on
# a COLD registry all miss the same memo and all bake the toolchain, filling
# the outer pool with 20-minute jobs until whatever is still queued dies on
# the pending timeout. Measured, right after std/refresh.sh re-keyed the
# std/cargo tree: `no runner for req (waited 900s)`. One publish first turns
# every later one into a tag hit.
#
# The test stack image travels to stage 4 as --image, because stage 4's own
# --result is the warm-up's report.
set -euo pipefail

caos get /cas/args/result
caos get /cas/args/workspace
caos get /cas/args/workspace/tests
caos get /cas/args/workspace/tests/lib
LIB=/cas/args/workspace/tests/lib

# The per-test map worker is curried HERE, where the image is a genuine
# --result tree, and travels to stage 4 as a REF STRING — the way every
# other worker ref moves (--run=, --map=). Passing the image itself as a
# curried arg does not work: `caos curry /cas/args/<argname>` curries over
# the arg NODE, so the resulting worker inherits this job's own bindings
# (observed: a map job whose args were `image in worker1`, running as uid
# 1000 because the image's root grant was not in play either).
map=$(caos curry /cas/args/result -- "--worker1:@=$LIB/run-test.sh")
fwd=(
  "--map=$map"
  "--workspace:@=/cas/args/workspace"
  "--build_ws:@=/cas/args/build_ws"
)
[ -e /cas/args/api_key ] && fwd+=("--api_key:@=/cas/args/api_key")
[ -e /cas/args/only ] && fwd+=("--only:@=/cas/args/only")

warm=$(caos curry /cas/args/result -- "--worker1:@=$LIB/warm-std.sh")
stage4=$(caos curry /cas/std/bash -- "--worker1:@=$LIB/suite-stage4.sh" "${fwd[@]}")
caos run-then /cas/args/workspace -- --run="$warm" --then="$stage4"
