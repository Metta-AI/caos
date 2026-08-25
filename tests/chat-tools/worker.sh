#!/usr/bin/env bash
# Inline file tools through the real chat/llm-step path. Five calls share one
# model response and no compute sub-run; mixed queues and grep are independent
# sibling tests rather than later turns in this conversation.
set -euo pipefail
# The dependency is mounted only inside the test wrapper and exports globals.
# shellcheck disable=SC1091
source DEEP-DEPS/llm-test/common.sh

stage "workspace and scripted inline-tool turn"
llm_test_setup
stub_host="${stub_host:?llm_test_setup did not set stub_host}"
mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
commit "workspace"
git config user.name tester
git config user.email tester@example.com
base=$(mkcommit "HEAD:ws" base)

INLINE_CALLS='[
 {"id":"tu_w","input":{"file_path":"notes/new.txt","content":"hello world"},"name":"write","type":"tool_use"},
 {"id":"tu_r","input":{"file_path":"notes/new.txt"},"name":"read","type":"tool_use"},
 {"id":"tu_e","input":{"file_path":"notes/new.txt","old_string":"hello","new_string":"goodbye"},"name":"edit","type":"tool_use"},
 {"id":"tu_x","input":{"file_path":"notes/new.txt","old_string":"never there","new_string":"x"},"name":"edit","type":"tool_use"},
 {"id":"tu_l","input":{"path":"notes"},"name":"ls","type":"tool_use"}]'
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$INLINE_CALLS" | tr -d '\n')" > stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"file tools done","type":"text"}],"stop_reason":"end_turn"}' \
  > stub/response-2.json
stub_pid=""
port=""
start_stub stub stub_pid port
stub_pid="${stub_pid:?start_stub did not set stub_pid}"

test_run_id="$(date +%s%N)-$$-$RANDOM"
conv="${test_run_id}-tools-inline"
conversation_ref="refs/caos/v2/conversations/$conv/head"
opts=(--model test-model --base-url "http://$stub_host:$port")

stage "write, read, edit, failed edit, and ls in one round"
"$CAOS_CLI" chat "$conv" -m "exercise the file tools" \
  --base "$base" "${opts[@]}" > inline.out
while IFS= read -r line; do echo "  inline| $line" >&2; done < inline.out
turn=$(remote_tip "$conversation_ref") || fail "inline-tool conversation has no head"
git fetch -q caos "$turn"
[ "$(git show "$turn:notes/new.txt")" = "goodbye world" ] \
  || fail "write+edit did not land in the turn tree"
[ "$(git show "$turn:notes/todo.txt")" = "hello notes" ] || fail "sibling file lost"
grep -qF "write notes/new.txt" inline.out || fail "write progress line missing"
grep -qF "read notes/new.txt" inline.out || fail "read progress line missing"
grep -qF "ls notes" inline.out || fail "ls progress line missing"
grep -qF '"hello world"' stub/request-2.json || fail "read result not sent"
grep -qF 'wrote notes/new.txt (11 bytes)' stub/request-2.json || fail "write result not sent"
grep -qF 'edited notes/new.txt (1 replacement)' stub/request-2.json || fail "edit result not sent"
grep -qF 'new.txt\ntodo.txt' stub/request-2.json || fail "ls listing not sent"
grep -qF '"is_error":true' stub/request-2.json || fail "bad edit not marked is_error"
grep -qF 'old_string not found' stub/request-2.json || fail "bad edit error not explained"
[ ! -f stub/request-3.json ] || fail "inline tools cost extra model rounds"

stage "done"
echo "chat-tools: ALL PASS" >&2
