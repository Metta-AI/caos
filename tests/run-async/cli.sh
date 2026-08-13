#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack (tests/lib/run-test.sh).
#
# The worker schedules its own in-flight ArgTree. If run-async waited for the
# result, the request would deadlock on itself; returning `request <Q>` proves
# the server took ownership before the worker exits and produces Q's result.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== run-async hands an in-flight request to the server ==" >&2
self=$("$CAOS_CLI" curry DEEP-DEPS/bash -- --worker1:@=test/self.sh)
reply=$("$CAOS_CLI" run "$self" --)

q=${reply#request }
if [ "$q" = "$reply" ] || [ "${#q}" -ne 40 ] || [[ ! "$q" =~ ^[0-9a-f]+$ ]]; then
  fail "expected 'request <40-character Q>', got: $reply"
fi

echo "  ok: the worker continued and returned the admitted request" >&2
echo "run-async: ALL PASS" >&2
