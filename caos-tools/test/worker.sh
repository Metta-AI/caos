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

caos get -r /cas/args/in || fail "materializing the workspace"
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
# The dev stack is TEST world, so a host client is refused by it and vice versa.
# It shares every dependency with the host build; only the thin workspace
# compile differs (measured: one derivation, 13.8s).
bins=$(nix build "path:$PWD#caos-test-world" --no-link --print-out-paths) \
  || fail "building the test-world binaries"
./dev/stack-up --bins="$bins" >&2 || fail "bringing the dev stack up"
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
if [ -e /cas/args/api-key ]; then
  caos get /cas/args/api-key
  args+=("--api-key:@=/cas/args/api-key")
fi

# THE REPORT IS A VALUE, red or green. SPEC is explicit that a tool's expected
# failures are results the model can read, and a failing suite is the single
# most expected failure this tool has. Only the harness breaking is an error.
#
# `--base:@=<path>` is how a client names an expression: it eval-paths the
# directory. A bare `run <path>` is not a form — `run` wants a base.
#
# NO COMMENT INSIDE THE BLOCK BELOW. Every line ends in a backslash, and a
# continuation followed by a comment joins INTO it — the env prefix is severed
# and the command runs with none of it. `bash -n` passes either way, which is
# what makes it worth a warning rather than a fix (CLAUDE.md). This exact block
# had it, and the symptom was a client with no server to talk to.
status=0
CAOS_SERVER_URL=http://127.0.0.1 \
  "$CLI" run /tmp/suite --base:@=dev/run-tests --in:@=. --cli:@=.caos-test-cli "${args[@]}" \
    >/tmp/run.out 2>/tmp/run.err || status=$?
if [ "$status" -ne 0 ]; then
  cat /tmp/run.err >&2
  fail "the suite did not produce a report (exit $status)"
fi

# THE WHOLE RESULT TREE, not just the report. SPEC's tool conventions: a tree
# with a `report` file has the report printed, and a FAILED banner in it marks
# the call a failure — while `results/<test>` stays addressable, which is what
# lets `test-result <hash>` read one test's full output.
caos put /tmp/suite /cas/out
