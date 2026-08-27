#!/bin/bash
# tests/chat-tools — a WORKER test, in dev/worker-test (it needs git).
#
# Inline file tools through the real llm-step path. Five calls share one model
# response and no compute sub-run; mixed queues and grep are independent sibling
# tests rather than later turns in this conversation.
#
# WHAT CHANGED WHEN THIS STOPPED BEING A CLIENT TEST. It used to type
# `caos-cli chat` and assert on the client's progress lines. `chat` is the
# client's turn loop — minting the human commit, publishing the admission,
# running the request, rendering progress — and none of that is llm-step. So
# the three steps that are plumbing are done here (worker-common.sh), and the
# assertions that were about the client's RENDERING are gone with it: what is
# left is the turn tree and what llm-step sent the model, which is the subject.
# The client's own progress output belongs to a test about the client.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "workspace and scripted inline-tool turn"
llm_test_setup

mkdir -p /tmp/ws/notes
echo "hello notes" > /tmp/ws/notes/todo.txt
caos put /tmp/ws /cas/ws >/dev/null || fail "publishing the workspace"
ws=$(caos hash /cas/ws)

INLINE_CALLS='[
 {"id":"tu_w","input":{"file_path":"notes/new.txt","content":"hello world"},"name":"write","type":"tool_use"},
 {"id":"tu_r","input":{"file_path":"notes/new.txt"},"name":"read","type":"tool_use"},
 {"id":"tu_e","input":{"file_path":"notes/new.txt","old_string":"hello","new_string":"goodbye"},"name":"edit","type":"tool_use"},
 {"id":"tu_x","input":{"file_path":"notes/new.txt","old_string":"never there","new_string":"x"},"name":"edit","type":"tool_use"},
 {"id":"tu_l","input":{"path":"notes"},"name":"ls","type":"tool_use"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$INLINE_CALLS" | tr -d '\n')" > /tmp/stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"file tools done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
stub_pid=""
port=""
start_stub /tmp/stub stub_pid port

new_llm_conversation tools-inline "$port" "$ws"

stage "write, read, edit, failed edit, and ls in one round"
dispatch_turn "$ws" "exercise the file tools"
turn=$(wait_turn) || {
  echo "--- stub log" >&2; cat /tmp/stub/log >&2 || true
  fail "the turn never advanced the conversation ref"
}
echo "  turn $turn" >&2

[ "$(git show "$turn:notes/new.txt")" = "goodbye world" ] \
  || fail "write+edit did not land in the turn tree"
[ "$(git show "$turn:notes/todo.txt")" = "hello notes" ] || fail "sibling file lost"

stage "and each tool's result reached the model"
grep -qF '"hello world"' /tmp/stub/request-2.json || fail "read result not sent"
grep -qF 'wrote notes/new.txt (11 bytes)' /tmp/stub/request-2.json \
  || fail "write result not sent"
grep -qF 'edited notes/new.txt (1 replacement)' /tmp/stub/request-2.json \
  || fail "edit result not sent"
grep -qF 'new.txt\ntodo.txt' /tmp/stub/request-2.json || fail "ls listing not sent"
grep -qF '"is_error":true' /tmp/stub/request-2.json || fail "bad edit not marked is_error"
grep -qF 'old_string not found' /tmp/stub/request-2.json \
  || fail "bad edit error not explained"
[ ! -f /tmp/stub/request-3.json ] || fail "inline tools cost extra model rounds"

stage "done"
printf 'chat-tools: ALL PASS\n' > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
