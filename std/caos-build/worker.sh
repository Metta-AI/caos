#!/usr/bin/env bash
# The `caos-build` tool: send `build` to the stack daemon for this tree.
#
# A ROUTER, like std/caos-test's: the build itself is the daemon's `build` op
# (dev/test-stack/worker.sh). It is a tool of its own rather than an op of
# `caos-stack` because a build is a pure function of the tree, so it wants the
# cache — a repeat is a hit — while `caos-stack`'s ops have effects and take a
# `request-id` to stay out of it.
set -euo pipefail

# THE ANSWER STAGE. The daemon answers with a `report` (or a blob); a `@root`
# tool answers with `{stdout, failed?}` (design/tool-contract.md), so the
# daemon's answer comes back here to be reshaped. A report that says FAILED
# marks the call failed, as the harness read it before.
if [ -e /cas/args/stage ]; then
  caos get /cas/args/result
  res=/tmp/caos-build-answer
  rm -rf "$res"; mkdir -p "$res"
  if [ -d /cas/args/result ]; then
    if [ -e /cas/args/result/report ] || [ -L /cas/args/result/report ]; then
      caos get /cas/args/result/report
      cp /cas/args/result/report "$res/stdout"
    else
      printf 'built: %s\n' "$(caos hash /cas/args/result)" > "$res/stdout"
    fi
  else
    cp /cas/args/result "$res/stdout"
  fi
  if grep -q FAILED "$res/stdout"; then
    printf 'the build failed\n' > "$res/failed"
  fi
  caos put "$res" /cas/out
  exit 0
fi

answer() {
  local res=/tmp/caos-build-answer
  rm -rf "$res"; mkdir -p "$res"
  cat > "$res/stdout"
  caos put "$res" /cas/out
  exit 0
}

# `root` is the tree to build (design/tool-contract.md): a source tree's
# commit resolves to its code tree, which is what the daemon is keyed by.
if [ ! -e /cas/args/root ] && [ ! -L /cas/args/root ]; then
  echo "caos-build: nothing is at the root path: root is the source tree to build, such as sources/caos/02-layer" | answer
fi
caos resolve "$(caos hash /cas/args/root)" "" /cas/caos-build-tree
in=/cas/caos-build-tree

# List the tree one level — enough to look for the codebase's own files.
caos get "$in" 2>/dev/null || true

# WRONG SOURCE TREE — a CLEAN RESULT, not an error. This tool is reachable as
# `caos-std/caos-build` from any session whose tree mounts caos, so it will be
# run on trees that are not caos. There it has nothing to build: rather than
# fail the turn, say so, so the model reads a calm "not applicable here"
# tool_result instead of a red one.
if [ ! -f "$in/flake.nix" ]; then
  { echo "caos-build compiles the caos source tree with nix, and this source tree"
    echo "has no flake.nix — there is nothing here for it to build."
    echo "caos-build is specific to the caos codebase; run it there."
  } | answer
fi

tree=$(caos hash "$in")
request=$(caos prepare-request --base:@=/cas/args/daemon \
  "--affinity=$tree" --op=build --in:@="$in")
then=$(caos curry --base:@=/cas/args/base --worker1:@=/cas/args/worker1 --stage=answer)
caos run-request-then "$request" --then:hash="$then"
