#!/usr/bin/env bash
# The `caos-build` tool: compile the tree with nix, in the dev image.
#
# It is an ORDINARY WORKER — no container of its own to launch, no engine to
# drive, no host paths to know. runnerd starts dev/test-stack, honours the volume grant
# that image declares, and `/worker` runs this. What makes the compile fast is
# that `/nix` is a persistent volume, so the second build is incremental against
# the first rather than starting from an empty store.
#
# NO STACK IS BROUGHT UP HERE. `nix build` needs nix and a source tree, nothing
# else; a dev stack is the `caos-test` tool's business.
set -euo pipefail

fail() { echo "BUILD FAIL: $*" >&2; exit 1; }

# The workspace, materialized. Cheap by measurement rather than by hope: the
# tracked tree is 335 files and 2.76 MB, so this is not a place to be clever.
caos get -r /cas/args/in || fail "materializing the workspace"
cd /cas/args/in

# WRONG SOURCE TREE — a CLEAN RESULT, not an error. caos-build is registered on
# every conversation (it is one of the harness's own tools), so it is offered
# even when the workspace is not caos. There it has nothing to build: rather
# than fail the turn, put a plain log saying so and exit 0, so the model reads a
# calm "not applicable here" tool_result instead of a red one.
if [ ! -f flake.nix ]; then
  { echo "caos-build compiles the caos source tree with nix, and this workspace"
    echo "has no flake.nix — there is nothing here for it to build."
    echo "caos-build is specific to the caos codebase; run it there."
  } > /tmp/build.log
  caos put /tmp/build.log /cas/out
  exit 0
fi

# NO `git init` HERE. A compile needs nix and a source tree; it is `caos-cli`
# that refuses to run outside a working tree, and nothing in this tool calls it.
# `dev/stack-up` makes the repo, because a repo is what a STACK needs — a client
# to push with and a `caos` remote to push to.
#
# `path:`, so nix takes the directory as it stands rather than looking for a git
# input in it. That is also what makes this correct either way: it behaves the
# same whether or not some later step has made this a repo.
status=0
nix build "path:$PWD" > /tmp/build.log 2>&1 || status=$?

# A FAILED BUILD IS A VALUE, not a job error. SPEC is explicit that a tool's
# expected failures are results the model can read and act on, and a build tool
# is called precisely when something might not compile. The banner goes LAST
# because both readers of a tool result truncate by keeping the tail.
if [ "$status" -ne 0 ]; then
  echo "BUILD FAILED (exit $status)" >> /tmp/build.log
else
  echo "BUILD OK" >> /tmp/build.log
fi

caos put /tmp/build.log /cas/out
