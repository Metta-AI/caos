#!/bin/bash
# The per-test worker1 (design/test-stack-image.md), mapped over by
# caos-tools/test.sh's fanout. Runs INSIDE a test stack: the image's /worker
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
# Nothing here publishes: the stack the image's /worker brought up already holds
# the deps this test declared, fetched by the interpreter. The expensive half is
# memoized in the host's registry by content (each std flake keyed on its tree
# hash), so the first suite pays the builds and every later one is a tag hit.
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
# `abort` — a VERDICT, not `infra`. Do not reclassify it: an unexpected abort is
# almost always the tree under test being wrong (a half-written cli.sh, a real
# bug in the feature the test drives) — deterministic, reproducible, and exactly
# what the author needs to read. Only the `|| infra "cargo worker did not run"`
# guards in tests/unit-* have ever meant "the environment broke".
#
# Caching an abort is cheap and self-clearing: a per-test job keys on the test's
# own tree AND the image digest, so fixing either re-keys it, and `--test-salt`
# forces a re-run when a genuine flake did cache. Calling it `infra` is not
# cheap: a job error fails the whole map, discarding every other test's result.
# (Tool sub-runs launch through `run-then --catch` — design/map-then.md — so a
# real `infra` error reaches the agent as an is_error tool_result rather than
# killing the turn.)
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

# The map child: {test, std, seed, workspace?, api-key?}, already materialized
# by the interpreter. Everything else a test runs on is in the image, which IS
# the tree under test.

# MATERIALIZE THE WRAPPER FIRST. Args arrive as lazy placeholders that are
# owner-only until fetched, so reading one without this gets "Permission
# denied" on a directory that looks like it is right there.
#
# This script does it because this script is the image's `/worker` now. It used
# to arrive already materialized: the test-stack interpreter ran
# `caos get -r /cas/args` before handing off to it, and that interpreter was
# deleted along with the image it interpreted.
caos get -r /cas/args/in || fail "materializing this test's wrapper"

TEST=/cas/args/in/test
[ -d "$TEST" ] || fail "no test tree at $TEST
  /cas/args:    $(ls -A /cas/args 2>&1 | tr '\n' ' ')
  /cas/args/in: $(ls -A /cas/args/in 2>&1 | tr '\n' ' ')"

# THE TESTED CLIENT, from this test's own wrapper. It used to come from the
# environment, set by the test-stack image's interpreter — which is gone, along
# with the image: a test is now an ordinary worker on the dev stack, and the
# thing that makes it a TEST of this tree rather than of some ambient caos is
# that the client it drives rode in as a content-addressed arg. So a test
# re-keys when the client changes, which is the property a suite exists to have.
#
# INSTALLED, not run in place: materialized CAS content is read-only and
# owner-only, so exec'ing straight out of /cas is "Permission denied".
caos get /cas/args/in/cli || fail "no tested client in this wrapper"
install -m 755 /cas/args/in/cli /tmp/caos-cli || fail "installing the tested client"
CAOS_CLI=/tmp/caos-cli
export CAOS_CLI
# CAOS_SERVER_URL is runnerd's, and runnerd here is the DEV stack's — so it
# already points at the stack under test. Nothing to flip.
: "${CAOS_SERVER_URL:?run-test needs CAOS_SERVER_URL from the runner}"

# The client repo the test's cli.sh snapshots from, staged exactly as
# tests/run.sh does: the test tree's contents at ./test.
mkdir -p /tmp/client && cd /tmp/client
git init -q .
git config user.email test@caos
git config user.name caos
git config gc.auto 0
git remote add caos "$CAOS_SERVER_URL"

# NO ALTERNATE OBJECT STORE, and if a push here ever dies unable to read an
# object, this is the first place to look.
#
# There used to be one: `/tmp/seed-git/objects`, the repo the old test-stack
# interpreter fetched a wrapper's deps into. It existed because a client pushes
# a request by walking the arg tree's closure, and git can only skip an object
# the remote advertises by traversing that ref LOCALLY — so a client holding a
# resolved image's HASH but not its objects died on the first one it could not
# read.
#
# Two things changed. There is no interpreter to fetch that repo, and the stack
# a test pushes to is the dev stack, which already holds every object it
# resolved. So the alternate has nothing to add — and CLAUDE.md is explicit that
# it also SUBTRACTS: it hides object-availability bugs, and `tests/push-closure`
# had to delete it to reproduce at all. Starting without one means that test is
# doing its job again.

# Tests that dogfood the workspace (cargo-self) get it in their wrapper as
# the git repo their cli.sh snapshots via $CAOS_PROJECT.
if [ -e /cas/args/in/workspace ]; then
  mkdir -p /tmp/ws && cp -r /cas/args/in/workspace/. /tmp/ws/
  git -C /tmp/ws init -q
  git -C /tmp/ws add -A
  git -C /tmp/ws -c user.email=test@caos -c user.name=caos commit -qm workspace
  export CAOS_PROJECT=/tmp/ws
fi

# CAOS_STUB_HOST points workers at in-job stub servers. THIS CONTAINER'S OWN
# ADDRESS ON THE NETWORK, deliberately, and not `127.0.0.1`.
#
# Loopback happens to work while every worker is a sibling in this container's
# netns, and stops working the moment they are not — which is exactly what
# design/faster-tests.md does: with one test stack shared by every test, the
# workers are launched into THAT stack's netns, and `127.0.0.1` would be the
# stack rather than the test that scripted the stub. A routable address is
# right under both arrangements (a netns can reach its own address), so this
# does not wait for that change and cannot become a mystery failure inside it.
#
# Read from /etc/hosts, in bash. The container runtime writes a line mapping
# this container's hostname to its address, and there is no `hostname`,
# `getent`, `ip` or `ifconfig` in this image to ask instead (checked) — an
# absent binary is exit 127 at runtime, not a build error.
#
# The stub binds 0.0.0.0 already (every caller passes it), so nothing on the
# test side changes.
self=$(cat /etc/hostname) || fail "no /etc/hostname; cannot find this container's address"
CAOS_STUB_HOST=""
while read -r ip names; do
  case " $names " in *" $self "*) CAOS_STUB_HOST=$ip; break ;; esac
done < /etc/hosts
[ -n "$CAOS_STUB_HOST" ] \
  || fail "no /etc/hosts entry for $self — a stub server would be unreachable"
export CAOS_STUB_HOST
echo "run-test: stubs reachable at $CAOS_STUB_HOST" >&2
#
# A test that EXECs something out of its own mounts must `install` a copy first:
# materialized CAS content is read-only and owner-only, so running it straight
# out of /cas is "Permission denied".
# A real-API test's key arrives in its wrapper (chat-online; absent = its
# cli.sh self-skips).
if [ -e /cas/args/in/api-key ]; then
  ANTHROPIC_API_KEY=$(cat /cas/args/in/api-key)
  export ANTHROPIC_API_KEY
fi

cp -r "$TEST" ./test

# The test's declared dependencies, at DEEP-DEPS/ in its own repo — the mount
# names its `DEPS` file used, and the same vocabulary any consumer uses.
#
# They have to be IN the repo (not just in the wrapper) because the CLI ingests
# git-tracked paths: `caos-cli run --base:@=DEEP-DEPS/rgrep` resolves by INGESTING that
# directory and then EVALUATING its `.caos-expr`. A dependency the repo does not
# hold is one the CLI cannot name, which is the point — the test's DEPS is the
# whole of what it can reach.
#
# `cp -rL` because CAS content is a tree of read-only, hash-tagged files; the
# copy is small now that every entry is source (the runner is a `{.caos-expr}`
# sentinel, not a 200MB delta).
if [ -d /cas/args/in/std ]; then
  mkdir -p DEEP-DEPS
  cp -rL /cas/args/in/std/. DEEP-DEPS/
  chmod -R u+w DEEP-DEPS
fi

git add -A && git commit -qm testtree
mkdir /tmp/out
# This test's own elapsed, for its line in the report — which test is the long
# pole. Only a DURATION: absolute times were here once, so the summariser could
# reconstruct the phase as max(end) - min(start), and that cannot work. They are
# files in the result, so a cache hit replays the pair from whenever the test
# last ran and the span reaches back to that run. The phase is stamped by the
# `fanout` stage instead, across jobs that really ran.
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

# The COMPLETE record rides in the result tree — the test's full output — so
# the suite result holds everything a debugger, human or agent, would want to
# read. No streaming, no archaeology: address the byte you need by path. The
# stack's own logs are NOT here any more: there is one dev stack shared by the
# suite, so its log is the stack's to keep, not each test's to copy.
cp /tmp/test.out /tmp/out/output
# `phases` is the interpreter's clock for everything BEFORE this script ran —
# arg materialization, finding the stack, fetching deps. None of it is in the
# `seconds` below, which starts once the stack is in hand, so without this the
# suite's per-test times describe a fraction of what a test job costs.
for log in server runnerd redis serve phases; do
  # `|| continue`, not `&& cp`: this loop is the last statement in the
  # script, so under set -e a missing final log would fail the whole job.
  [ -e "/tmp/$log.log" ] || continue
  cp "/tmp/$log.log" "/tmp/out/$log.log"
done


# THE RESULT, published here rather than by an interpreter. This script is the
# image's `/worker` now, so there is nothing above it in the container to do
# this — that was `test-stack/worker`'s last job before it was deleted.
caos put /tmp/out /cas/out
