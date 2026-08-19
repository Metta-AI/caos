#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack (tests/lib/run-test.sh).
#
# The worker schedules its own in-flight ArgTree. If run-async waited for the
# result, the request would deadlock on itself. The eventual result below proves
# that the ordinary /run request continues after the worker disconnects.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
object_status() { # <oid>
  curl -sS -o /dev/null -w '%{http_code}' -I "$CAOS_SERVER_URL/object/$1"
}

echo "== run-async dispatches an in-flight request without waiting ==" >&2
self=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/self.sh)
reply=$("$CAOS_CLI" run --base:hash="$self")

q=${reply#request }
if [ "$q" = "$reply" ] || [ "${#q}" -ne 40 ] || [[ ! "$q" =~ ^[0-9a-f]+$ ]]; then
  fail "expected 'request <40-character Q>', got: $reply"
fi

echo "  ok: the worker continued after dispatching the request" >&2

echo "== dispatched work finishes after its caller exits ==" >&2
delayed=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash --worker1:@=test/delayed.sh)
payload="background-finished-$(date +%s%N)-$$-$RANDOM"
expected_content="completed: $payload"
expected_oid=$(printf '%s\n' "$expected_content" | git hash-object --stdin)
q=$("$CAOS_CLI" prepare-request --base:hash="$delayed" --payload="$payload")
[ "${#q}" -eq 40 ] && [[ "$q" =~ ^[0-9a-f]+$ ]] \
  || fail "prepare-request returned malformed Q: $q"

launcher=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/bash \
  --worker1:@=test/launch.sh --request="$q")
dispatched=$("$CAOS_CLI" run --base:hash="$launcher")
[ "$dispatched" = "request $q" ] \
  || fail "launcher dispatched the wrong request: $dispatched"

# There is no blocking /run Q here: the launcher container is already gone.
# The result content is unique to this run, so only the disconnected request
# can make its known object id addressable through the core object API.
complete=0
for _ in $(seq 1 150); do
  status=""
  if ! status=$(object_status "$expected_oid"); then
    fail "server unreachable while waiting for dispatched request $q"
  fi
  case "$status" in
    200) complete=1; break ;;
    404) ;;
    *) infra "server returned HTTP $status while waiting for dispatched request $q" ;;
  esac
  sleep 0.1
done
[ "$complete" -eq 1 ] || fail "dispatched request $q never stored its result"

"$CAOS_CLI" run actual --base:hash="$q" >/dev/null
[ "$(cat actual)" = "$expected_content" ] \
  || fail "background result contents were wrong: $(cat actual)"
echo "  ok: result became addressable with no foreground Q caller" >&2

echo "run-async: ALL PASS" >&2
