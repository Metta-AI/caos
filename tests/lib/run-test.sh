#!/bin/bash
# The per-test worker1 (design/test-stack-image.md), mapped over by
# caos-tools/test.sh's stage3. Runs INSIDE a test stack: the image's /worker
# has already brought up the inner server, runnerd and a private redis, and put
# the tree's binaries first on PATH with CAOS_SERVER_URL aimed at them.
#
# It is the ONE stage of the suite that does not live in caos-tools/test.sh
# with the others, and cannot: everything below runs in the inner world, where
# `caos` is the tested client aimed at the inner server, so the stage-dispatch
# line at the top of that file — a `caos get` of its own args — would ask the
# INNER server for an outer object and 404.
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
#
# But ONLY a test outcome is a value. An INFRASTRUCTURE failure — the inner
# stack, a runner, a tool the test relied on — is a job error: loud and
# UNCACHED (design/cargo-workers.md), so a re-run retries instead of replaying a
# memoised red. The whole distinction rides on cli.sh's exit code, a two-word
# contract the harness (the BASH_ENV file below) sources into every cli.sh:
#
#   exit 0       the test passed                         -> PASS verdict (value)
#   fail / 1     the thing UNDER TEST was wrong          -> FAIL verdict (value)
#   infra / *    the environment BROKE                   -> job error (uncached)
#
# `fail` is each cli.sh's own helper (exit 1); `infra` the harness provides. A
# test that GUARDS a risky step says which broke — `... || infra "no runner"`
# vs `... || fail "tests failed"`. And an UNGUARDED command that trips `set -e`
# lands on the ERR trap, i.e. defaults to `infra`: an unexpected abort is
# environment, never the test's verdict, so it can never cache as a spurious
# red. Getting the split wrong only ever costs a re-run, never a false green.
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

# The map child: {test, workspace?, api-key?}, already materialized by the
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
#
# The binaries arrive in the wrapper — only the ones this test declared in
# uses-bin — but they cannot be RUN from /cas: materialized content is
# read-only and owner-only by design, so a test that execs one straight out of
# the CAS gets "Permission denied" (llm-stub, measured). They used to live at
# /caos/bin inside the image, mode 755, which is why nothing noticed. Stage a
# real executable copy and point CAOS_BIN_DIR at that, so the contract tests
# see is exactly what it was.
mkdir -p /tmp/bin
if [ -d /cas/args/in/bin ]; then
  for b in /cas/args/in/bin/*; do
    [ -e "$b" ] || continue
    install -m 755 "$b" "/tmp/bin/$(basename "$b")"
  done
fi
export CAOS_BIN_DIR=/tmp/bin
export CAOS_STUB_HOST=127.0.0.1
# A real-API test's key arrives in its wrapper (chat-online; absent = its
# cli.sh self-skips).
if [ -e /cas/args/in/api-key ]; then
  ANTHROPIC_API_KEY=$(cat /cas/args/in/api-key)
  export ANTHROPIC_API_KEY
fi

cp -r "$TEST" ./test
git add -A && git commit -qm testtree
mkdir /tmp/out
# This test's own elapsed, for its line in the report — which test is the long
# pole. Only a DURATION: absolute times were here once, so the summariser could
# reconstruct the phase as max(end) - min(start), and that cannot work. They are
# files in the result, so a cache hit replays the pair from whenever the test
# last ran and the span reaches back to that run. The phase is stamped by
# stage3 instead, across jobs that really ran.
t0=$SECONDS

# The pass/fail/infra vocabulary, sourced into cli.sh via BASH_ENV so it lives
# in ONE place and no tests/<name>/cli.sh has to declare it. `set -o errtrace`
# so the ERR trap reaches into the test's own functions (its `commit`, its
# helpers); the trap turns an UNGUARDED `set -e` abort into `infra` (exit 70)
# instead of the failed command's own code — which, being 1, would be
# indistinguishable from a deliberate `fail`. `infra` is also callable directly,
# for the guard points that catch a run failure themselves (unit-test's
# `|| ok=0`), where the harness cannot see the abort.
cat > /tmp/harness.sh <<'HARNESS'
set -o errtrace
infra() { echo "RUN-TEST INFRA: ${*:-cli.sh aborted unexpectedly}" >&2; exit 70; }
trap 'infra "cli.sh aborted (rc=$?) — a command expected to succeed did not"' ERR
# Confine the contract to the top-level cli.sh: unexport BASH_ENV so the helper
# shells and worker scripts a test spawns do NOT re-source this and inherit the
# errtrace + ERR trap. The current shell keeps them (already sourced); only
# descendants are spared, so nothing downstream changes behaviour.
unset BASH_ENV
HARNESS

# cli.sh's exit code IS the verdict (see the header): 0 pass, 1 fail, anything
# else infra. `|| rc=$?` so this script's own `set -e` does not abort on a
# non-zero test — we are classifying it, not propagating it.
rc=0
BASH_ENV=/tmp/harness.sh bash test/cli.sh >/tmp/test.out 2>&1 || rc=$?
cat /tmp/test.out >&2

case "$rc" in
  0) echo "RUN-TEST: PASS" > /tmp/out/verdict ;;
  1) echo "RUN-TEST: FAIL" > /tmp/out/verdict ;;
  # An infra failure is NOT a verdict. `fail` exits this worker non-zero, so the
  # job errors: the result is not cached (a re-run retries) and the failure is
  # loud — it fails the whole map, taking the summary with it, rather than
  # hiding under a cached red. The test's own output is already on stderr above;
  # fail adds the inner stack's logs.
  *) fail "cli.sh exited $rc — an infrastructure failure, not a test verdict" ;;
esac
echo $((SECONDS - t0)) > /tmp/out/seconds

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

