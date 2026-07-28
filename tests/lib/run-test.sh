#!/bin/bash
# The per-test worker1 (design/test-stack-image.md). Runs INSIDE a test
# stack: the image's /worker has already brought up the inner server,
# runnerd and a private redis, and put the tree's binaries first on PATH
# with CAOS_SERVER_URL aimed at them.
#
# This script lives ENTIRELY in the inner world: `caos-cli` on PATH is the
# tested client and CAOS_SERVER_URL is the stack it brought up. There is no
# reaching the outer stack from here and no need to — both clients read the
# same CAOS_SERVER_URL, so the interpreter materialized this job's args
# before flipping the env, and publishes the result tree we leave at
# /tmp/out afterwards.
#
# The inner std is published by the tree's OWN build-builtins.sh — the same
# script the host runs — so every std image is built by this stack from this
# tree. The expensive half is memoized in the host's registry by content
# (each std flake keyed on its tree hash), so the first suite pays the builds
# and every later one is a tag hit.
#
# The test's OUTCOME is a value, not a job error: the result tree carries the
# verdict, the test's full output and the inner stack's logs, so one failing
# test never hides the others' results and a failure caches like any result
# (same inputs, same failure; salt to retry a flake).
set -euo pipefail

fail() {
  echo "RUN-TEST FAIL: $*" >&2
  for l in /tmp/server.log /tmp/runnerd.log /tmp/publish.log; do
    [ -e "$l" ] || continue
    echo "--- $l" >&2
    cat "$l" >&2
  done
  exit 1
}

# The map child: {test, workspace?, api_key?}, already materialized by the
# interpreter. Binaries and images no longer ride in the wrapper — they are
# in the image, which IS the tree under test.

TEST=/cas/args/in/test
[ -d "$TEST" ] || fail "no test tree at $TEST"

# The inner std, published by the tree's own publisher. REGISTRY_HTTP: the
# docker daemon this delegates to is the OUTER one, for which the registry is
# localhost:5000, while this container is on caos-net and must call it
# caos-registry:5000 — one registry, two names (design/test-stack-image.md).
CAOS_CLI=/caos/bin/caos-cli \
CAOS_CLIENT_REPO=/tmp/publish-client-repo \
CAOS_BUILTIN_IMAGES="$(echo /caos/images/*.tar.gz)" \
CAOS_BUILTIN_BINS=/caos \
CAOS_REGISTRY_HTTP=caos-registry:5000 \
  bash /caos/tree/build-builtins.sh >/tmp/publish.log 2>&1 \
  || fail "publishing the inner std"

# The client repo the test's cli.sh snapshots from, staged exactly as
# tests/run.sh does: the test tree's contents at ./test.
mkdir -p /tmp/client && cd /tmp/client
git init -q .
git config user.email test@caos
git config user.name caos
git config gc.auto 0
git remote add caos "$CAOS_SERVER_URL"

# Tests that dogfood the workspace (cargo-self) get it in their wrapper as
# the git repo their cli.sh snapshots via $CAOS_PROJECT.
if [ -e /cas/args/in/workspace ]; then
  mkdir -p /tmp/ws && cp -r /cas/args/in/workspace/. /tmp/ws/
  git -C /tmp/ws init -q
  git -C /tmp/ws add -A
  git -C /tmp/ws -c user.email=test@caos -c user.name=caos commit -qm workspace
  export CAOS_PROJECT=/tmp/ws
fi

# CAOS_BIN_DIR hands the tests their helper binaries (they would otherwise
# shell out to host nix); CAOS_STUB_HOST points workers at in-job stub
# servers — siblings share this container's netns, so localhost is the
# stub's address, not the engine host.
export CAOS_BIN_DIR=/caos/bin
export CAOS_STUB_HOST=127.0.0.1
# A real-API test's key arrives in its wrapper (chat-online; absent = its
# cli.sh self-skips).
if [ -e /cas/args/in/api_key ]; then
  ANTHROPIC_API_KEY=$(cat /cas/args/in/api_key)
  export ANTHROPIC_API_KEY
fi

cp -r "$TEST" ./test
git add -A && git commit -qm testtree
mkdir /tmp/out
if bash test/cli.sh >/tmp/test.out 2>&1; then
  echo "RUN-TEST: PASS" > /tmp/out/verdict
else
  echo "RUN-TEST: FAIL" > /tmp/out/verdict
fi
cat /tmp/test.out >&2

# The COMPLETE record rides in the result tree — the test's full output, the
# std publish, and the inner stack's logs — so the suite result holds
# everything a debugger, human or agent, would want to read. No streaming, no
# archaeology: address the byte you need by path.
cp /tmp/test.out /tmp/out/output
for log in server runnerd redis publish; do
  # `|| continue`, not `&& cp`: this loop is the last statement in the
  # script, so under set -e a missing final log would fail the whole job.
  [ -e "/tmp/$log.log" ] || continue
  cp "/tmp/$log.log" "/tmp/out/$log.log"
done

