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

echo "== admitted work finishes after its caller exits ==" >&2
delayed=$("$CAOS_CLI" curry DEEP-DEPS/bash -- --worker1:@=test/delayed.sh)
q=$("$CAOS_CLI" prepare-request "$delayed" -- --payload=background-finished)
[ "${#q}" -eq 40 ] && [[ "$q" =~ ^[0-9a-f]+$ ]] \
  || fail "prepare-request returned malformed Q: $q"

launcher=$("$CAOS_CLI" curry DEEP-DEPS/bash -- \
  --worker1:@=test/launch.sh --request="$q")
admitted=$("$CAOS_CLI" run "$launcher" --)
[ "$admitted" = "request $q" ] \
  || fail "launcher admitted the wrong request: $admitted"

# There is no blocking /run Q here: the launcher container is already gone.
# Only the server-owned background run can publish the eventual result ref.
complete=0
for _ in $(seq 1 150); do
  if git ls-remote --exit-code --refs caos "refs/caos/res/$q" >/dev/null 2>&1; then
    complete=1
    break
  fi
  sleep 0.1
done
[ "$complete" -eq 1 ] || fail "server-owned request $q never published a result"

"$CAOS_CLI" run "$q" actual -- >/dev/null
[ "$(cat actual)" = background-finished ] \
  || fail "background result contents were wrong: $(cat actual)"
echo "  ok: result became addressable with no foreground Q caller" >&2

echo "run-async: ALL PASS" >&2
