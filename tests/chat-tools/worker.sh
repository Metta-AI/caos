#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "workspace and scripted inline-tool turn"
llm_test_setup

mkdir -p /tmp/ws/notes /tmp/ws/files
echo "hello notes" > /tmp/ws/notes/todo.txt
echo "hello files" > /tmp/ws/files/todo.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the workspace")

INLINE_CALLS='[
 {"id":"tu_w","input":{"workspace":"main","file-path":"files/new.txt","content":"hello world"},"name":"write","type":"tool_use"},
 {"id":"tu_r","input":{"workspace":"main","file-path":"files/new.txt"},"name":"read","type":"tool_use"},
 {"id":"tu_e","input":{"workspace":"main","file-path":"files/new.txt","old-string":"hello","new-string":"goodbye"},"name":"edit","type":"tool_use"},
 {"id":"tu_x","input":{"workspace":"main","file-path":"files/new.txt","old-string":"never there","new-string":"x"},"name":"edit","type":"tool_use"},
 {"id":"tu_l","input":{"workspace":"main","path":"files/"},"name":"ls","type":"tool_use"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$INLINE_CALLS" | tr -d '\n')" > /tmp/stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"file tools done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
start_stub /tmp/stub

new_llm_conversation tools-inline "$STUB_PORT" "$ws"
dispatch_turn "exercise the file tools"
wait_turn || fail "the turn never reached a terminal head"

workspace=$(workspace_commit "$head")
[ "$workspace" != "$base" ] || fail "inline mutations did not advance main"
fetch_code "$workspace" "fetching the resulting workspace"
[ "$(git show "$workspace:files/new.txt")" = "goodbye world" ] \
  || fail "write+edit did not land in main"
[ "$(git show "$workspace:notes/todo.txt")" = "hello notes" ] \
  || fail "sibling file lost"

$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/tools.jsonl
jq -s -e '
  length == 5 and
  (map(.id) | sort) == (["tu_w","tu_r","tu_e","tu_x","tu_l"] | sort) and
  all(.[]; .status == "complete" and .task == null and .workspace_name == "main")
' /tmp/tools.jsonl >/dev/null || { cat /tmp/tools.jsonl >&2; fail "inline tool records are wrong"; }
[ "$($TOOL parents --repo /tmp/repo --head "$head" | grep -c ' tool.complete$')" -eq 5 ] \
  || fail "inline calls did not append five tool.complete transitions"
$TOOL tool-observation --repo /tmp/repo --head "$head" --request "$request" \
  --round 0 --id tu_x > /tmp/failed-edit.json
grep -qF 'old-string not found' /tmp/failed-edit.json \
  || fail "bad edit observation lacks its error"
grep -qF '"is_error":true' /tmp/failed-edit.json \
  || fail "bad edit observation is not marked is_error"

grep -qF '"hello world"' /tmp/stub/request-2.json || fail "read result not sent"
grep -qF 'wrote files/new.txt (11 bytes)' /tmp/stub/request-2.json \
  || fail "write result not sent"
grep -qF 'edited files/new.txt (1 replacement)' /tmp/stub/request-2.json \
  || fail "edit result not sent"
grep -qF 'new.txt\ntodo.txt' /tmp/stub/request-2.json || fail "ls listing not sent"
grep -qF '"is_error":true' /tmp/stub/request-2.json || fail "bad edit not marked is_error"
grep -qF 'old-string not found' /tmp/stub/request-2.json \
  || fail "bad edit error not explained"
[ ! -f /tmp/stub/request-3.json ] || fail "inline tools cost extra model rounds"

stage "conversation files work without a workspace"
mkdir -p /tmp/stub-zero
printf '%s\n' \
  '{"content":[{"id":"tu_no_workspace","input":{"file-path":"plan.md","content":"wrong place"},"name":"write","type":"tool_use"},{"id":"tu_files","input":{"file-path":"files/plan.md","content":"right place"},"name":"write","type":"tool_use"}],"stop_reason":"tool_use"}' \
  > /tmp/stub-zero/response-1.json
printf '%s\n' \
  '{"content":[{"text":"zero workspace done","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub-zero/response-2.json
zero_pid=""
zero_port=""
start_stub /tmp/stub-zero zero_pid zero_port
new_llm_conversation tools-zero "$zero_port" - "You are a coding agent." "" \
  "currying zero-workspace llm-step"
dispatch_turn "exercise files without a workspace"
wait_turn || fail "the zero-workspace turn did not finish"
$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/zero-tools.jsonl
jq -s -e 'any(.[]; .id == "tu_no_workspace" and .status == "failed")' \
  /tmp/zero-tools.jsonl >/dev/null || fail "workspace-less write was not rejected"
$TOOL tool-observation --repo /tmp/repo --head "$head" --request "$request" \
  --round 0 --id tu_no_workspace > /tmp/no-workspace.json
grep -qF 'no workspace' /tmp/no-workspace.json \
  || fail "workspace-less write did not explain the missing workspace"
[ "$(record "$head" files/plan.md)" = "right place" ] \
  || fail "conversation-files write did not land"

pass chat-tools
