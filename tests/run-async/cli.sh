#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack (tests/lib/run-test.sh).
#
# The worker schedules its own in-flight ArgTree. If run-async waited for the
# result, the request would deadlock on itself. The eventual result below proves
# that the ordinary /run request continues after the worker disconnects.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
remote_ref_status() { # <ref>
  curl -sS -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' \
    --data "{\"ref\":\"$1\"}" "$CAOS_SERVER_URL/ref/read"
}

echo "== run-async dispatches an in-flight request without waiting ==" >&2
self=$("$CAOS_CLI" curry DEEP-DEPS/bash -- --worker1:@=test/self.sh)
reply=$("$CAOS_CLI" run "$self" --)

q=${reply#request }
if [ "$q" = "$reply" ] || [ "${#q}" -ne 40 ] || [[ ! "$q" =~ ^[0-9a-f]+$ ]]; then
  fail "expected 'request <40-character Q>', got: $reply"
fi

echo "  ok: the worker continued after dispatching the request" >&2

echo "== dispatched work finishes after its caller exits ==" >&2
delayed=$("$CAOS_CLI" curry DEEP-DEPS/bash -- --worker1:@=test/delayed.sh)
q=$("$CAOS_CLI" prepare-request "$delayed" -- --payload=background-finished)
[ "${#q}" -eq 40 ] && [[ "$q" =~ ^[0-9a-f]+$ ]] \
  || fail "prepare-request returned malformed Q: $q"

launcher=$("$CAOS_CLI" curry DEEP-DEPS/bash -- \
  --worker1:@=test/launch.sh --request="$q")
dispatched=$("$CAOS_CLI" run "$launcher" --)
[ "$dispatched" = "request $q" ] \
  || fail "launcher dispatched the wrong request: $dispatched"

# There is no blocking /run Q here: the launcher container is already gone.
# Only the disconnected /run request can publish the eventual result ref.
complete=0
for _ in $(seq 1 150); do
  status=""
  if ! status=$(remote_ref_status "refs/caos/res/$q"); then
    infra "server unreachable while waiting for dispatched request $q"
  fi
  case "$status" in
    200) complete=1; break ;;
    404) ;;
    *) infra "server returned HTTP $status while waiting for dispatched request $q" ;;
  esac
  sleep 0.1
done
[ "$complete" -eq 1 ] || fail "dispatched request $q never published a result"

"$CAOS_CLI" run "$q" actual -- >/dev/null
[ "$(cat actual)" = background-finished ] \
  || fail "background result contents were wrong: $(cat actual)"
echo "  ok: result became addressable with no foreground Q caller" >&2

echo "run-async: ALL PASS" >&2
