#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "search fixture and scripted grep turn"
llm_test_setup

mkdir -p /tmp/ws/notes
echo "hello notes" > /tmp/ws/notes/todo.txt
echo "goodbye world" > /tmp/ws/notes/new.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the search fixture")

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
start_stub /tmp/stub

new_llm_conversation tools-grep "$STUB_PORT" "$ws"
dispatch_turn "search the workspace"
wait_turn || fail "the grep turn never reached a terminal head"

[ "$(workspace_commit "$head")" = "$base" ] || fail "grep changed the main pointer"
$TOOL tools --repo /tmp/repo --head "$head" --request "$request" > /tmp/grep-tools.jsonl
jq -s -e '
  length == 3 and
  all(.[]; .workspace_name == "main" and .result.proposal == null)
' /tmp/grep-tools.jsonl >/dev/null || fail "grep records have wrong workspace metadata"

grep -qF 'notes/todo.txt:1:hello notes' /tmp/stub/request-2.json \
  || fail "root grep match not sent"
grep -qF 'notes/new.txt:1:goodbye world' /tmp/stub/request-2.json \
  || fail "scoped grep match not sent"
grep -qF '"is_error":true' /tmp/stub/request-2.json \
  || fail "invalid pattern not marked is_error"
grep -qF 'invalid pattern' /tmp/stub/request-2.json \
  || fail "invalid pattern error not explained"
[ ! -f /tmp/stub/request-3.json ] || fail "unexpected extra model round"

pass chat-tools-grep
