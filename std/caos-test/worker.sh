#!/usr/bin/env bash
# The `caos-test` tool: build the tree, stand a dev stack up in this container,
# and run the suite on it.
#
# TWO LEVELS OF CAOS, and keeping them straight is the whole shape of this file:
#
#   OUT HERE   this script is a worker on the host stack. `/bin/caos` is the
#              host's client, `/cas/args` is the host's, and the result it puts
#              goes back to the host.
#   IN THERE   `dev/run-tests` is a job on the DEV stack — the one `stack-up`
#              just started from the tree under test — driven by the client that
#              tree just compiled. That is where the per-test fan-out and the
#              per-test caching live.
#
# So the suite's jobs key on the dev stack's world, not the host's: a test that
# has not changed is a cache hit THERE, and this outer job is a cache hit here
# whenever the source tree is unchanged. Two layers, each keyed on what it
# actually depends on.
#
# WHY THE SUITE IS NOT RUN OUT HERE. It has to drive the code under test, which
# means a stack built from the tree — and the host stack is not that, it is the
# stack the agent asking for the tests is sitting in. Restarting the host to
# test a change is exactly the coupling this design removes.
set -euo pipefail

fail() { echo "TEST FAIL: $*" >&2; exit 1; }

# PHASE MARKERS. 70 of a 75-second single-test run was not the test, and the
# split was invisible: everything up to the suite happens inside this one
# container, so the trace shows one node and nothing about what it spent the
# time on. `T0` is this job's start; each `phase` line is seconds since it.
T0=$SECONDS
phase() { echo "==> [+$((SECONDS - T0))s] $*" >&2; }

caos get -r /cas/args/in || fail "materializing the source tree"
phase "materialized the source tree"
cd /cas/args/in

# WRONG SOURCE TREE — a CLEAN RESULT, not an error. caos-test is registered on
# every conversation (it is one of the harness's own tools), so it is offered
# even when the source tree is not caos. There it has no stack to build and no
# suite to run: rather than fail the turn, put a plain note saying so and exit 0,
# so the model reads a calm "not applicable here" tool_result instead of a red
# one. A plain blob renders through the same tool conventions as the report.
if [ ! -f flake.nix ] || [ ! -x dev/stack-up ]; then
  { echo "caos-test builds a caos dev stack from the tree and runs the caos suite"
    echo "on it. This source tree is not the caos codebase (no flake.nix / dev/stack-up),"
    echo "so there is nothing here for it to test."
    echo "caos-test is specific to the caos codebase; run it there."
  } > /tmp/not-caos
  caos put /tmp/not-caos /cas/out
  exit 0
fi

# The stack, and a client for it. `stack-up` compiles the tree, brings the
# daemons up as children of THIS container, publishes std, and leaves the
# client at /caos-run/bin.
#
# `/caos-run` IS THIS CONTAINER'S, and that is the whole of why it is not
# `/caos-dev`, which is a volume every dev stack on this host shares. Several
# of these tools run at once, each testing a different tree; the fixed path
# under the volume used to be repointed by whichever started last, so the first
# then drove ITS OWN server with the OTHER build's caos-cli. dev/stack-up's
# header has the rest.
#
# Its failure is an INFRASTRUCTURE failure, not a test verdict: nothing was
# tested, so a caller must retry rather than read a red report. Hence the bare
# exit rather than a report with a FAILED banner.
# ONE nix build, for everything the stack needs: `.#caos-test-stack-inputs`
# carries the daemons and the worker images in a single derivation, so stack-up
# resolves nothing. The dev stack is TEST world, so a host client is refused by
# it and vice versa.
# It shares every dependency with the host build; only the thin source tree
# compile differs (measured: one derivation, 13.8s).
phase "building the stack inputs"
inputs=$(nix build "path:$PWD#caos-test-stack-inputs" --no-link --print-out-paths) \
  || fail "building the stack inputs"
phase "bringing the dev stack up"
./dev/stack-up --inputs="$inputs" >&2 || fail "bringing the dev stack up"
CLI=/caos-run/bin/caos-cli
[ -x "$CLI" ] || fail "no client at $CLI after stack-up"

# THE TESTED CLIENT GOES INTO THE SOURCE TREE. `:@=` ingests git-tracked paths
# inside the worktree and nothing else, so a client sitting in /caos-run is
# rejected as "outside the git worktree".
#
# Copying it in is not a workaround, it is the honest shape: the client is part
# of what the suite TESTS, so a tree that carries it is a tree that re-keys when
# it changes — which is exactly the property a suite has to have. `add -f`,
# because .gitignore knows nothing about this path.
install -m 755 "$CLI" ./.caos-test-cli || fail "staging the tested client"
git add -f ./.caos-test-cli || fail "tracking the tested client"

# The suite. `--test-salt` re-keys every per-test job on the dev stack, and —
# via dev/cli-test, which exports it as the inner client's CAOS_SALT — the
# requests a client test makes. Nothing ABOVE a test moves, so the compile and
# the std publish stay hits. CAOS_SALT cannot be used for this: set here it
# threads into every sub-run, scaffolding included.
args=()
if [ -e /cas/args/test-salt ]; then
  caos get /cas/args/test-salt
  args+=("--test-salt=$(cat /cas/args/test-salt)")
fi
if [ -e /cas/args/only ]; then
  caos get /cas/args/only
  args+=("--only=$(cat /cas/args/only)")
fi
# HOW MANY TESTS RUN AT ONCE. Passed through to the suite's `map-then`, which is
# the only place that can bound it — a runner-slot cap bounds CONTAINERS, and a
# test waiting on a sub-run holds none. Absent means all of them, which is what
# this always did.
if [ -e /cas/args/max-parallel ]; then
  caos get /cas/args/max-parallel
  args+=("--max-parallel=$(cat /cas/args/max-parallel)")
fi

# THE TREE THE SUITE RUNS IS A COMMIT THE DEV STACK HOLDS, so a secret can be
# granted to a path in it by locator (SPEC, "Secrets"). Committed at a fixed
# date, like stack-up's, so an unchanged tree is an unchanged commit.
GIT_AUTHOR_DATE="@0 +0000" GIT_COMMITTER_DATE="@0 +0000" \
  git -c user.name=caos -c user.email=dev@caos commit -q --allow-empty -m "tested client" \
  || fail "committing the tested client"
git push -q http://127.0.0.1 "HEAD:refs/heads/caos-test/$(git rev-parse HEAD)" \
  || fail "pushing the workspace commit to the dev stack"

# THE SUITE'S WRITER (design/ref-writers.md): the dev stack enforces who may
# write a ref, so the client driving the suite signs as a writer, and the suite
# hands that on (`--writes=*`) to the tests and the steps they run. A fresh key
# per run: it is in no ArgTree, so it moves no cache key.
writer=$("$CLI" ref-writer-key new 2>/dev/null) || fail "creating the suite's ref writer key"
git config caos.ref-writer-key "$writer"
args+=("--writes=*")

# A MOCK KEY FOR std/llm-call, std/llm-step, dev/worker-test and std/go — not a secret:
# the value is a constant, and the only thing that ever sees it is a stub HTTP
# server a test starts in its own container. Granted here, the suite's own run
# carries it, so a worker test forms an llm-call request directly and the key
# arrives at /secret. Tests reach these through DEEP-DEPS copies, which carry
# the origin of the node they copy.
#
# dev/worker-test and std/go are granted so a test's own ArgTree carries the
# key's `secret-hash`: llm-step's admission protocol names the exact request
# hash in advance, and a worker can only form that request if it can bind the
# entry. std/go is the image the Go worker tests run on.
STORE=/tmp/caos-test-secrets
key=$("$CLI" secrets-init --dir="$STORE") || fail "creating the test store"
git config caos.secret-readers "$key"
at="git+http://caos.invalid/caos-test?rev=$(git rev-parse HEAD)&dir"
{
  printf 'name=anthropic-api-key\n'
  printf 'value=mock-key-for-the-llm-call-stub\n'
  printf 'entropy=fedcba9876543210fedcba9876543210\n'
  printf 'reader:@@=%s=std/llm-call\n' "$at"
  printf 'reader:@@=%s=std/llm-step\n' "$at"
  printf 'reader:@@=%s=dev/worker-test\n' "$at"
  printf 'reader:@@=%s=std/go\n' "$at"
} > "$STORE/llm-mock"
"$CLI" secrets-push --dir="$STORE" --server=http://127.0.0.1 >/dev/null || fail "pushing the test store"
echo "==> granting llm-call, llm-step, dev/worker-test and std/go a mock key" >&2

# THE REPORT IS A VALUE, red or green. SPEC is explicit that a tool's expected
# failures are results the model can read, and a failing suite is the single
# most expected failure this tool has. Only the harness breaking is an error.
#
# `--base:@=<path>` is how a client names an expression: it eval-paths the
# directory. A bare `run <path>` is not a form — `run` wants a base.
#
# THE SUITE'S REQUEST, NAMED BEFORE IT RUNS, so the two stacks' traces join up.
#
# The dev stack writes its trace records to the SAME redis as the host — a trace
# key carries no cache namespace, unlike a result — so both sets of records
# already sit side by side. What was missing was an edge from this job to the
# suite's, and `caos trace-child` records exactly that and nothing else.
#
# `prepare-request` forms and pushes the very ArgTree the `run` below will form,
# so the hash is known before any work starts — which is the point: this is for
# watching a suite that is still running.
#
# NO COMMENT INSIDE THE BLOCK BELOW (see the warning further down).
phase "forming the suite request"
suite_req=$(CAOS_SERVER_URL=http://127.0.0.1 \
  "$CLI" prepare-request --base:@=dev/run-tests --in:@=. --cli:@=.caos-test-cli "${args[@]}") \
  || fail "forming the suite request"
caos trace-child suite "$suite_req" || fail "linking the suite's trace to this job"
echo "==> suite request $suite_req (caos-cli status --all $suite_req)" >&2
# THE RAW TRACE, fetchable from the HOST after this is all over. A trace key
# carries no cache namespace, so the dev stack's records land in the same redis
# the host server reads — which is why an address inside this container is not
# what to print. `all=1` is the COMPLETE view (View::Complete): the live one
# elides finished work, and after a run that is all of it.
#
# Printed rather than left to be reconstructed: the suite's request hash is
# knowable only here, and without it the trace is unreachable.
echo "==> full trace JSON:" >&2
echo "    curl -s localhost:9090/status/$suite_req?all=1 | jless" >&2

phase "running the suite"
status=0
CAOS_SERVER_URL=http://127.0.0.1 \
  "$CLI" run /tmp/suite --base:@=dev/run-tests --in:@=. --cli:@=.caos-test-cli "${args[@]}" \
    >/tmp/run.out 2>/tmp/run.err || status=$?
if [ "$status" -ne 0 ]; then
  cat /tmp/run.err >&2
  fail "the suite did not produce a report (exit $status)"
fi

# THE TRACE COMMAND GOES IN THE REPORT, not just on stderr above: this job's
# stderr is relayed only when it FAILS, and the run you most want to take apart
# is a green one that was slower than it should have been.
#
# The result tree is a checkout, so its files are read-only.
chmod u+w /tmp/suite/report
{ echo
  echo "full trace (the host reads the dev stack's records — a trace key carries"
  echo "no cache namespace, so both stacks write to the one redis):"
  echo "  curl -s localhost:9090/status/$suite_req?all=1 | jless"
  echo
  echo "and while a run is in flight, CAOS_WATCH_LINES=0 shows every node"
  echo "instead of the first 16."
  echo
  if [ -f "/caos-run/logs/runnerd.log" ]; then
    echo "runnerd: $(grep -c 'running job' /caos-run/logs/runnerd.log || true) claimed," \
      "$(wc -l < /caos-run/logs/runnerd.log) lines"
    echo "this stack's logs, on the host: /caos-dev/runs/$(cat /etc/hostname)/logs"
  fi
} >> /tmp/suite/report

# THE WHOLE RESULT TREE, not just the report. SPEC's tool conventions: a tree
# with a `report` file has the report printed, and a FAILED banner in it marks
# the call a failure — while `results/<test>` stays addressable, which is what
# lets `caos-test-result <hash>` read one test's full output.
phase "done"
caos put /tmp/suite /cas/out
