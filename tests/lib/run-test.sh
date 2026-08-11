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
# memoised red. The whole distinction rides on cli.sh's exit code, a contract
# the harness (the BASH_ENV file below) sources into every cli.sh:
#
#   exit 0       the test passed                         -> PASS verdict (value)
#   fail / 1     the thing UNDER TEST was wrong          -> FAIL verdict (value)
#   abort / 2    cli.sh tripped `set -e` unexpectedly    -> FAIL verdict (value)
#   infra / *    the environment BROKE                   -> job error (uncached)
#
# `fail` is each cli.sh's own helper (exit 1); `abort` and `infra` the harness
# provides. A test that GUARDS a risky step says which broke — `... || infra
# "no runner"` vs `... || fail "tests failed"`.
#
# An UNGUARDED command that trips `set -e` lands on the ERR trap, which calls
# `abort` — a VERDICT. It used to call `infra`, on the reasoning that an
# unexpected abort is environment and so must never cache as a spurious red.
# That reasoning was wrong on both halves, measured twice in a row: an agent's
# half-written cli.sh, and a genuine push bug in a half-built feature the test
# was driving. Both were the tree under test being wrong — deterministic,
# reproducible, and exactly what the author needed to read — and both got filed
# as "the environment broke". Only the four `|| infra "cargo worker did not
# run"` guards in tests/unit-* have ever meant it.
#
# The cost of caching an abort is small and self-clearing: a per-test job keys
# on the test's own tree AND the image digest, so fixing either the cli.sh or
# the code it drives re-keys the job. `--test-salt` forces a re-run when a
# genuine flake did cache.
#
# The cost of the old default was not small. A job error fails the whole map,
# taking the summary with it — so one aborted cli.sh discarded every other
# test's result, errored the `test` tool's sub-run, and killed the agent turn
# that called it. This header used to end "getting the split wrong only ever
# costs a re-run, never a false green", which was not true while it cost a turn.
#
# It is true again, and not only because of the reclassification above: tool
# sub-runs now launch through `run-then --catch` (design/map-then.md), so even a
# real `infra` job error reaches the agent as an is_error tool_result over an
# unchanged workspace instead of killing the turn. Both halves matter — this one
# puts aborts in the right bucket, that one makes the wrong bucket survivable.
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

# The seed repo as an ALTERNATE object store, so the client can READ std's
# objects without fetching them.
#
# A client never fetches the std tree — `std_tree` says so and means it: a tree
# fetch pulls the whole closure, which is how you pull a 1.5GB rustc image to
# resolve one name. Evaluation fetches only what it WALKS. But an arg tree
# carries `std` whole, and a rustc-built tool's image IS the runner delta, so a
# push has to traverse objects nothing ever walked into (`runner/layer00`) and
# git dies on the first one it cannot read — even though the server already has
# it and advertises a ref covering it, because git can only mark an advertised
# tip uninteresting by traversing it LOCALLY.
#
# The objects are already on this filesystem: /tmp/seed-git is what the
# interpreter fetched the wrapper's std into, one directory away. Pointing at it
# costs nothing and moves no bytes — and it stays honest about the subset,
# because the seed repo holds exactly what this test declared.
if [ -d /tmp/seed-git/objects ]; then
  echo /tmp/seed-git/objects > /tmp/client/.git/objects/info/alternates
fi

# Tests that dogfood the workspace (cargo-self) get it in their wrapper as
# the git repo their cli.sh snapshots via $CAOS_PROJECT.
if [ -e /cas/args/in/workspace ]; then
  mkdir -p /tmp/ws && cp -r /cas/args/in/workspace/. /tmp/ws/
  git -C /tmp/ws init -q
  git -C /tmp/ws add -A
  git -C /tmp/ws -c user.email=test@caos -c user.name=caos commit -qm workspace
  export CAOS_PROJECT=/tmp/ws
fi

# CAOS_STUB_HOST points workers at in-job stub servers — siblings share this
# container's netns, so localhost is the stub's address, not the engine host.
#
# There is NO CAOS_BIN_DIR any more. It handed tests host binaries staged into
# the wrapper by the suite, which meant a second delivery mechanism alongside
# std — and exactly one binary was ever named through it (llm-stub). That is a
# std entry now, built by cargo rather than rustc because it is a plain sidecar
# process and not a worker image, so a test declares it in DEPS like anything
# else and takes it out of its own mount. The read-only-CAS lesson survives in
# the tests that do so: materialized content is read-only and owner-only, so a
# test `install`s a real executable copy before running it ("Permission denied"
# straight out of /cas, measured).
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

# The pass/fail/abort/infra vocabulary, sourced into cli.sh via BASH_ENV so it
# lives in ONE place and no tests/<name>/cli.sh has to declare it. `set -o
# errtrace` so the ERR trap reaches into the test's own functions (its `commit`,
# its helpers); the trap turns an UNGUARDED `set -e` abort into `abort`
# (exit 2) instead of the failed command's own code — which, being 1, would be
# indistinguishable from a deliberate `fail`, and the report says which it was.
# `infra` stays callable directly, for the guard points that catch a run failure
# themselves (unit-test's `|| ok=0`), where the harness cannot see the abort.
cat > /tmp/harness.sh <<'HARNESS'
set -o errtrace
abort() { echo "RUN-TEST ABORT: ${*:-cli.sh aborted unexpectedly}" >&2; exit 2; }
infra() { echo "RUN-TEST INFRA: ${*:-the environment broke}" >&2; exit 70; }
trap 'abort "cli.sh aborted (rc=$?) — a command expected to succeed did not"' ERR
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
  # An unguarded `set -e` abort: still a verdict, but a distinct one, so the
  # report can say "this test never reached an assertion" rather than implying
  # the assertion ran and disagreed. The summariser reads the parenthetical.
  2) echo "RUN-TEST: FAIL (aborted)" > /tmp/out/verdict ;;
  # An infra failure is NOT a verdict — now only ever an explicit `infra` call.
  # `fail` exits this worker non-zero, so the job errors: the result is not
  # cached (a re-run retries) and the failure is loud — it fails the whole map,
  # taking the summary with it, rather than hiding under a cached red. The
  # test's own output is already on stderr above; fail adds the stack's logs.
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

