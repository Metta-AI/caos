#!/usr/bin/env bash
# The `caos-build` tool: send `build` to the stack daemon for this tree.
#
# A ROUTER, like std/caos-test's: the build itself is the daemon's `build` op
# (dev/stack-daemon/worker.sh). It is a tool of its own rather than an op of
# `caos-stack` because a build is a pure function of the tree, so it wants the
# cache — a repeat is a hit — while `caos-stack`'s ops have effects and take a
# `request-id` to stay out of it.
set -euo pipefail

# List the tree one level — enough to look for the codebase's own files.
caos get /cas/args/in

# WRONG SOURCE TREE — a CLEAN RESULT, not an error. This tool is reachable as
# `caos-std/caos-build` from any conversation whose tree mounts caos, and `in`
# defaults to whatever tree the path selected, so it will be run on trees that
# are not caos. There it has nothing to build: rather than fail the turn, say so,
# so the model reads a calm "not applicable here" tool_result instead of a red one.
if [ ! -f /cas/args/in/flake.nix ]; then
  { echo "caos-build compiles the caos source tree with nix, and this source tree"
    echo "has no flake.nix — there is nothing here for it to build."
    echo "caos-build is specific to the caos codebase; run it there."
  } > /tmp/build.log
  caos put /tmp/build.log /cas/out
  exit 0
fi

tree=$(caos hash /cas/args/in)
request=$(caos prepare-request --base:@=/cas/args/daemon \
  "--affinity=$tree" --op=build --in:@=/cas/args/in)
caos run-request-then "$request"
