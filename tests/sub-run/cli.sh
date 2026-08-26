#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack (tests/lib/run-test.sh).
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI)
# ----------------------------------------------------------
# The subject here IS the sub-run dispatch path of the tested client: its
# `curry` / `prepare-request` / `run` / `sub-run` verbs and the request-wire
# contract they speak to the inner server. Nothing this test pins down exists
# outside that interaction, so the CLI is essential, not incidental — a plain
# reimplementation would exercise none of the tested code and prove nothing.
#
# Concretely, the two properties are only observable by driving real work
# through the client against the inner stack:
#
#   1. NON-BLOCKING DISPATCH. `caos sub-run <Q>` must return `request <Q>` and
#      let its worker keep running, rather than waiting on the result. The
#      worker below (self.sh) schedules its OWN in-flight ArgTree: a blocking
#      implementation would wait on this worker's own result and deadlock on
#      itself. The server admits the recursive edge under the current stack,
#      where it fails independently. The verdict is the exact wire reply the
#      client parses back — `request <40-hex Q>` — which only the tested client
#      can produce, and which is the whole point of the feature.
#
#   2. FIRE-AND-FORGET COMPLETION. Work the client dispatches must finish and
#      become addressable through the core object API AFTER its caller
#      container is gone (launch.sh dispatches Q, then exits). With no
#      foreground Q caller left, the dispatched result's known object id can
#      only appear if the client's `prepare-request` + `sub-run` actually
#      handed the server a self-standing request. Polling the object API for
#      that id, then reading it back with `caos run`, tests precisely the
#      client's dispatch-and-detach behaviour end to end.
#
# So every command routes through $CAOS_CLI deliberately: the tested client's
# dispatch protocol is the thing under test.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
object_status() { # <oid>
  curl -sS -o /dev/null -w '%{http_code}' -I "$CAOS_SERVER_URL/object/$1"
}

echo "== sub-run dispatches an in-flight request without waiting ==" >&2
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

# There is no blocking caller for Q here: the launcher container is already gone.
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
    *) fail "server returned HTTP $status while waiting for dispatched request $q" ;;
  esac
  sleep 0.1
done
[ "$complete" -eq 1 ] || fail "dispatched request $q never stored its result"

"$CAOS_CLI" run actual --base:hash="$q" >/dev/null
[ "$(cat actual)" = "$expected_content" ] \
  || fail "background result contents were wrong: $(cat actual)"
echo "  ok: result became addressable with no foreground Q caller" >&2

echo "sub-run: ALL PASS" >&2
