#!/bin/bash
# tests/chat-tools-grep — a WORKER test, in dev/worker-test (it needs git).
#
# Grep's llm-step integration: root and subtree dispatch plus an invalid
# pattern. Purpose-built files replace two unrelated prior chat turns.
#
# These assertions are deliberately about what the MODEL saw, not how it got
# there — which is what let them survive grep moving out of llm-step into
# `std/rgrep-tool` unchanged. The invalid pattern is no longer prechecked by
# the harness; the tool reports FAILED and the generic renderer marks it
# is_error, and the assertion below cannot tell the difference. That is the
# point.
#
# The client used to type `caos-cli chat`; that is the client's turn loop, not
# llm-step's grep, and std/llm-test/worker-common.sh does those steps here. The
# assertions about the client's progress RENDERING are gone with it — what is
# left is the turn tree and what llm-step sent the model.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "search fixture and scripted grep turn"
llm_test_setup

mkdir -p /tmp/ws/notes
echo "hello notes" > /tmp/ws/notes/todo.txt
echo "goodbye world" > /tmp/ws/notes/new.txt
caos put /tmp/ws /cas/ws >/dev/null || fail "publishing the search fixture"
ws=$(caos hash /cas/ws)

GREP_CALLS='[
 {"id":"tu_g1","input":{"pattern":"hello"},"name":"grep","type":"tool_use"},
 {"id":"tu_g2","input":{"pattern":"goodbye","path":"notes"},"name":"grep","type":"tool_use"},
 {"id":"tu_g3","input":{"pattern":"("},"name":"grep","type":"tool_use"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$GREP_CALLS" | tr -d '\n')" > /tmp/stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"grep done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
stub_pid=""
port=""
start_stub /tmp/stub stub_pid port

new_llm_conversation tools-grep "$port" "$ws"

stage "root, scoped, and invalid-pattern grep"
dispatch_turn "$ws" "search the workspace"
turn=$(wait_turn) || {
  echo "--- stub log" >&2; cat /tmp/stub/log >&2 || true
  fail "the turn never reached a terminal event"
}
echo "  turn $turn" >&2

# GREP READS; it must not write. The turn's tree is compared against the
# workspace it started from.
[ "$(git rev-parse "$turn^{tree}")" = "$ws" ] || fail "grep changed the workspace tree"

stage "and each result reached the model"
grep -qF 'notes/todo.txt:1:hello notes' /tmp/stub/request-2.json \
  || fail "root grep match not sent"
grep -qF 'notes/new.txt:1:goodbye world' /tmp/stub/request-2.json \
  || fail "scoped grep match not sent"
grep -qF '"is_error":true' /tmp/stub/request-2.json \
  || fail "invalid pattern not marked is_error"
grep -qF 'invalid pattern' /tmp/stub/request-2.json \
  || fail "invalid pattern error not explained"
[ ! -f /tmp/stub/request-3.json ] || fail "unexpected extra model round"

stage "done"
printf 'chat-tools-grep: ALL PASS\n' > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
