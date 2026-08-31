#!/bin/bash
# tests/chat-tools-mixed — a WORKER test, in dev/worker-test (it needs git).
#
# A mixed tool queue: inline write, bash compute sub-run, then an inline edit.
# The ordering assertion is the point; it needs no prior turns.
#
# The client used to type `caos-cli chat`; that is the client's turn loop, not
# llm-step's queue ordering, and std/llm-test/worker-common.sh does those steps
# here. The assertions about the client's progress RENDERING are gone with it.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "workspace and scripted mixed-tool turn"
llm_test_setup

mkdir -p /tmp/ws
echo "fixture" > /tmp/ws/original.txt
caos put /tmp/ws /cas/ws >/dev/null || fail "publishing the workspace"
ws=$(caos hash /cas/ws)

MIXED_CALLS='[
 {"id":"tu_mw","input":{"file-path":"mix.txt","content":"hello"},"name":"write","type":"tool_use"},
 {"id":"tu_mb","input":{"cmd":"tr a-z A-Z < mix.txt > mix3.txt","paths":["mix.txt"]},"name":"bash","type":"tool_use"},
 {"id":"tu_me","input":{"file-path":"mix.txt","old-string":"hello","new-string":"world"},"name":"edit","type":"tool_use"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$MIXED_CALLS" | tr -d '\n')" > /tmp/stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"mixed done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
stub_pid=""
port=""
start_stub /tmp/stub stub_pid port

new_llm_conversation tools-mixed "$port" "$ws"

stage "inline write, bash, and inline edit stay ordered"
dispatch_turn "$ws" "mix inline and bash"
turn=$(wait_turn) || {
  echo "--- stub log" >&2; cat /tmp/stub/log >&2 || true
  fail "the turn never reached a terminal event"
}
echo "  turn $turn" >&2

# The bash sub-run must have seen the inline write that preceded it, and the
# inline edit must have landed on top of the bash run's workspace.
[ "$(git show "$turn:mix.txt")" = "world" ] || fail "post-bash edit did not land"
[ "$(git show "$turn:mix3.txt")" = "HELLO" ] || fail "bash did not see inline write"

# ...and the RESULTS went back to the model in call order.
sequence=$(grep -o '"tool_use_id":"tu_m[wbe]"' /tmp/stub/request-2.json \
  | grep -o 'tu_m[wbe]' | paste -sd,)
[ "$sequence" = "tu_mw,tu_mb,tu_me" ] \
  || fail "results missing or misordered: $sequence"
[ ! -f /tmp/stub/request-3.json ] || fail "unexpected extra model round"

stage "done"
printf 'chat-tools-mixed: ALL PASS\n' > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
