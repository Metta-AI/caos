#!/bin/bash
# The per-test worker1 (design/test-stack-image.md). Runs INSIDE a test
# stack: the image's /worker has already brought up the inner server,
# runnerd and a private redis, and put the tree's binaries first on PATH
# with CAOS_SERVER_URL aimed at them.
#
# This script lives ENTIRELY in the inner world: `caos-cli` on PATH is the
# TESTED client — a different build from the host's, in general, which is why
# it must be the one every test command goes through — and CAOS_SERVER_URL is
# the stack it brought up. Nothing here reaches the outer stack, and nothing
# needs to: the interpreter materialized this job's args before flipping the
# env and publishes the tree we leave at /tmp/out afterwards. Do not reach for
# /bin/caos to "get at the outer stack" — that is the host's client, and using
# it here would either 404 (its calls go to the inner server too) or, worse,
# quietly run host code where the tested code was the point.
#
# The inner std is published by the tree's OWN build-builtins.sh — the same
# script the host runs — but ONCE, when the image was built (the seed,
# design/one-stack-image.md), not once per test. So nothing here publishes:
# the stack /worker brought up already answers refs/caos/std. The expensive
# half is still memoized in the host's registry by content (each std flake
# keyed on its tree hash), so the first suite pays the builds and every later
# one is a tag hit.
#
# The test's OUTCOME is a value, not a job error: the result tree carries the
# verdict, the test's full output and the inner stack's logs, so one failing
# test never hides the others' results and a failure caches like any result
# (same inputs, same failure; salt to retry a flake).
set -euo pipefail

fail() {
  echo "RUN-TEST FAIL: $*" >&2
  for l in /tmp/server.log /tmp/runnerd.log; do
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
[ -d "$TEST" ] || fail "no test tree at $TEST
  /cas/args:    $(ls -A /cas/args 2>&1 | tr '\n' ' ')
  /cas/args/in: $(ls -A /cas/args/in 2>&1 | tr '\n' ' ')"

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

# The COMPLETE record rides in the result tree — the test's full output and
# the inner stack's logs — so the suite result holds everything a debugger,
# human or agent, would want to read. No streaming, no archaeology: address
# the byte you need by path.
cp /tmp/test.out /tmp/out/output
for log in server runnerd redis serve; do
  # `|| continue`, not `&& cp`: this loop is the last statement in the
  # script, so under set -e a missing final log would fail the whole job.
  [ -e "/tmp/$log.log" ] || continue
  cp "/tmp/$log.log" "/tmp/out/$log.log"
done

