#!/usr/bin/env bash
# The `caos-test` tool: send `run-tests` to the stack daemon for this tree.
#
# THE ROUTER. What this tool used to do — build the tree, stand a dev stack up in
# the container, run the suite on it — is the daemon's `run-tests` op now
# (dev/test-stack/worker.sh, which has the long account of how). This script
# only addresses the message: the daemon for a tree is keyed by that tree's oid,
# so every `caos-test` of one tree, whatever its --only or --test-salt, reaches
# the one container that already has the tree's stack up.
#
# `affinity` AND `in` BOTH NAME THE TREE. The server routes on the first and the
# daemon materializes the second; the daemon refuses a message where they
# disagree, so a hand-formed one cannot reach a stack built from another tree.
#
# CACHED LIKE ANY JOB. The message's ArgTree is the key, so the same tree, salt
# and selection answer from the cache without reaching the daemon at all, and a
# new --test-salt is a new message to the SAME daemon. That is what the
# `request-id` of `caos-stack` is for and this tool does not need: a repeated
# suite run is meant to be a hit.
set -euo pipefail

# List the tree one level — enough to look for the codebase's own files.
caos get /cas/args/in
caos get /cas/args/in/dev 2>/dev/null || true

# WRONG SOURCE TREE — a CLEAN RESULT, not an error. caos-test is registered on
# every conversation (it is one of the harness's own tools), so it is offered
# even when the source tree is not caos. There it has no stack to build and no
# suite to run: rather than fail the turn, put a plain note saying so and exit 0,
# so the model reads a calm "not applicable here" tool_result instead of a red
# one. A plain blob renders through the same tool conventions as the report.
if [ ! -f /cas/args/in/flake.nix ] || [ ! -e /cas/args/in/dev/stack-up ]; then
  { echo "caos-test builds a caos dev stack from the tree and runs the caos suite"
    echo "on it. This source tree is not the caos codebase (no flake.nix / dev/stack-up),"
    echo "so there is nothing here for it to test."
    echo "caos-test is specific to the caos codebase; run it there."
  } > /tmp/not-caos
  caos put /tmp/not-caos /cas/out
  exit 0
fi

tree=$(caos hash /cas/args/in)
forward=()
for name in test-salt only max-parallel; do
  if [ -e "/cas/args/$name" ]; then forward+=("--$name:@=/cas/args/$name"); fi
done

request=$(caos prepare-request --base:@=/cas/args/daemon \
  "--affinity=$tree" --op=run-tests --in:@=/cas/args/in "${forward[@]}")
caos run-request-then "$request"
