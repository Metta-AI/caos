#!/usr/bin/env bash
# Grep's llm-step integration: root and subtree dispatch plus invalid-pattern
# preflight. Purpose-built files replace two unrelated prior chat turns.
set -euo pipefail
# The dependency is mounted only inside the test wrapper and exports globals.
# shellcheck disable=SC1091
source DEEP-DEPS/llm-test/common.sh

stage "search fixture and scripted grep turn"
llm_test_setup
stub_host="${stub_host:?llm_test_setup did not set stub_host}"
mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
echo "goodbye world" > ws/notes/new.txt
commit "search fixture"
git config user.name tester
git config user.email tester@example.com
base=$(mkcommit "HEAD:ws" base)

GREP_CALLS='[
 {"id":"tu_g1","input":{"pattern":"hello"},"name":"grep","type":"tool_use"},
 {"id":"tu_g2","input":{"pattern":"goodbye","path":"notes"},"name":"grep","type":"tool_use"},
 {"id":"tu_g3","input":{"pattern":"("},"name":"grep","type":"tool_use"}]'
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$GREP_CALLS" | tr -d '\n')" > stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"grep done","type":"text"}],"stop_reason":"end_turn"}' \
  > stub/response-2.json
stub_pid=""
port=""
start_stub stub stub_pid port
stub_pid="${stub_pid:?start_stub did not set stub_pid}"

test_run_id="$(date +%s%N)-$$-$RANDOM"
conv="${test_run_id}-tools-grep"
conversation_ref="refs/caos/v2/conversations/$conv/head"
opts=(--model test-model --base-url "http://$stub_host:$port")

stage "root, scoped, and invalid-pattern grep"
"$CAOS_CLI" chat "$conv" -m "search the workspace" \
  --base "$base" "${opts[@]}" > grep.out
while IFS= read -r line; do echo "  grep| $line" >&2; done < grep.out
turn=$(remote_tip "$conversation_ref") || fail "grep conversation has no head"
git fetch -q caos "$turn"
git diff --quiet "$base" "$turn" -- || fail "grep changed the workspace tree"
grep -qF "grep hello" grep.out || fail "root grep progress line missing"
grep -qF "grep goodbye notes" grep.out || fail "scoped grep progress line missing"
grep -qF 'notes/todo.txt:1:hello notes' stub/request-2.json \
  || fail "root grep match not sent"
grep -qF 'notes/new.txt:1:goodbye world' stub/request-2.json \
  || fail "scoped grep match not sent"
grep -qF '"is_error":true' stub/request-2.json \
  || fail "invalid pattern not marked is_error"
grep -qF 'invalid pattern' stub/request-2.json || fail "invalid pattern error not explained"
[ ! -f stub/request-3.json ] || fail "unexpected extra model round"

stage "done"
echo "chat-tools-grep: ALL PASS" >&2
