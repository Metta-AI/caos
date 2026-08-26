#!/usr/bin/env bash
# The `test` tool: build the tree, stand a dev stack up in this container, and
# run the suite on it.
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
# whenever the workspace tree is unchanged. Two layers, each keyed on what it
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

caos get -r /cas/args/in || fail "materializing the workspace"
phase "materialized the workspace"
cd /cas/args/in
[ -f flake.nix ] || fail "no flake.nix in the workspace"
[ -x dev/stack-up ] || fail "no dev/stack-up in the workspace"

# The stack, and a client for it. `stack-up` compiles the tree, brings the
# daemons up as children of THIS container, publishes std, and leaves the
# client at /caos-dev/bin.
#
# Its failure is an INFRASTRUCTURE failure, not a test verdict: nothing was
# tested, so a caller must retry rather than read a red report. Hence the bare
# exit rather than a report with a FAILED banner.
# ONE nix build, for everything the stack needs: `.#caos-test-stack-inputs`
# carries the daemons and the worker images in a single derivation, so stack-up
# resolves nothing. The dev stack is TEST world, so a host client is refused by
# it and vice versa.
# It shares every dependency with the host build; only the thin workspace
# compile differs (measured: one derivation, 13.8s).
phase "building the stack inputs"
inputs=$(nix build "path:$PWD#caos-test-stack-inputs" --no-link --print-out-paths) \
  || fail "building the stack inputs"
phase "bringing the dev stack up"
./dev/stack-up --inputs="$inputs" >&2 || fail "bringing the dev stack up"
CLI=/caos-dev/bin/caos-cli
[ -x "$CLI" ] || fail "no client at $CLI after stack-up"

# THE TESTED CLIENT GOES INTO THE WORKSPACE. `:@=` ingests git-tracked paths
# inside the worktree and nothing else, so a client sitting in /caos-dev is
# rejected as "outside the git worktree".
#
# Copying it in is not a workaround, it is the honest shape: the client is part
# of what the suite TESTS, so a tree that carries it is a tree that re-keys when
# it changes — which is exactly the property a suite has to have. `add -f`,
# because .gitignore knows nothing about this path.
install -m 755 "$CLI" ./.caos-test-cli || fail "staging the tested client"
git add -f ./.caos-test-cli || fail "tracking the tested client"

# The suite. `--test-salt` re-keys every per-test job on the dev stack and
# nothing else, so the compile and the std publish stay hits — CAOS_SALT cannot
# do that, since it threads into every sub-run.
args=()
if [ -e /cas/args/test-salt ]; then
  caos get /cas/args/test-salt
  args+=("--test-salt=$(cat /cas/args/test-salt)")
fi
if [ -e /cas/args/only ]; then
  caos get /cas/args/only
  args+=("--only=$(cat /cas/args/only)")
fi

# A SECRET STORE DOES NOT CROSS A STACK. It is built by the client from its own
# `.caos-secrets` and shipped in a header, and the server grants a secret to any
# job whose ArgTree is a superset of a declared reader's — all within ONE call
# stack. tests/chat-online's real-API turn runs two stacks down, on the dev
# stack, so the value has to be re-registered there.
#
# That is what this does, and it is the one place in the tree where a secret is
# written to disk: it arrives here at /secret/anthropic-api-key because the
# caller's store names `caos-tools/test` as a reader, and it is written back as
# a store for the dev stack naming `tests/chat-online` as its reader. The value
# never enters an ArgTree or the CAS.
#
# AFTER `stack-up`, deliberately. stack-up git-inits this workspace with
# `add -Af` — the tree is the truth — so a `.caos-secrets` written before it
# would be COMMITTED and then ingested by `--in:@=.`. Written afterwards it is
# untracked, and `:@=` ingests only tracked paths.
#
# Absent, nothing is written and chat-online self-skips, which is what happens
# whenever the caller has no key or has not granted this tool.
if [ -e /secret/anthropic-api-key ]; then
  mkdir -p .caos-secrets
  {
    printf 'name=anthropic-api-key\n'
    printf 'value=%s\n' "$(cat /secret/anthropic-api-key)"
    printf 'entropy=0123456789abcdef0123456789abcdef\n'
    printf 'reader=tests/chat-online\n'
  } > .caos-secrets/anthropic-api-key
  chmod 600 .caos-secrets/anthropic-api-key
  echo "==> granting tests/chat-online the anthropic key on the dev stack" >&2
fi

# A MOCK KEY FOR std/llm-call, unconditionally — and it is not a secret: the
# value is a constant, and the only thing that ever sees it is a stub HTTP
# server the test starts in its own container.
#
# WHY IT IS INJECTED HERE. A secret store is built by a CLIENT from its own
# `.caos-secrets`, so a test that needs one used to have to BE a client — which
# is the only reason tests/llm-call staged a repo and drove caos-cli, to test a
# worker. Written here instead, the suite's own run carries it, and the server
# grants it to any job whose ArgTree is a superset of the reader's. So the test
# forms an llm-call request directly and the key arrives at /secret.
#
# A SECOND FILE, not a second `reader=` line on the one above. Both grants use
# the name `anthropic-api-key` (llm-call reads exactly that path) but must carry
# DIFFERENT VALUES: adding `reader=std/llm-call` to chat-online's file would POST
# the caller's real key to a stub, which then writes it to a request-N.json the
# test greps. The store is a list, matched per-reader and deduped by name, so two
# entries sharing a name are fine as long as no job matches both — and none can:
# a tests/chat-online job does not run the llm-call image.
#
# The entropy differs from chat-online's for the same reason it exists at all:
# it is the cache-isolation tag, and two values under one name must not share a
# cache key.
mkdir -p .caos-secrets
{
  printf 'name=anthropic-api-key\n'
  printf 'value=mock-key-for-the-llm-call-stub\n'
  printf 'entropy=fedcba9876543210fedcba9876543210\n'
  printf 'reader=std/llm-call\n'
  printf 'reader=std/llm-step\n'
  # AND THE TEST IMAGE ITSELF, which is what lets a worker test form a request
  # that can be granted anything. `caos prepare-request` in a worker folds NO
  # `secret-hash` (it has no store), and the server fail-closes without it — so
  # a request a test forms is refused, and one the server folds the entry into
  # has a hash the test cannot predict. Since all three readers here share one
  # name and one entropy, they imply the SAME digest: a test running in
  # dev/worker-test carries that entry in its own ArgTree and can bind it onto
  # the request it forms, which then matches what llm-step is dispatched as.
  # That is why llm-step's admission protocol — which requires naming the exact
  # request hash in advance — works from a worker at all.
  printf 'reader=dev/worker-test\n'
} > .caos-secrets/llm-mock
chmod 600 .caos-secrets/llm-mock
echo "==> granting llm-call, llm-step and dev/worker-test a mock key" >&2

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
echo "    curl -s localhost:9090/status/$suite_req?all=1 | jq ." >&2

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
  echo "  curl -s localhost:9090/status/$suite_req?all=1 | jq ."
  echo
  echo "and while a run is in flight, CAOS_WATCH_LINES=0 shows every node"
  echo "instead of the first 16."
} >> /tmp/suite/report

# THE WHOLE RESULT TREE, not just the report. SPEC's tool conventions: a tree
# with a `report` file has the report printed, and a FAILED banner in it marks
# the call a failure — while `results/<test>` stays addressable, which is what
# lets `test-result <hash>` read one test's full output.
phase "done"
caos put /tmp/suite /cas/out
