#!/bin/bash
# tests/llm-subagent — a WORKER test, in dev/worker-test (it needs git).
#
# Subagents are ordinary durable conversations: spawning returns stable
# identifiers, and the child inherits a clean workspace and human owner.
# Everything asserted here lives in refs on the server, which is the point —
# the client was only currying the parent turn and blocking on it.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "workspace and scripted parent/child model"
llm_test_setup

rm -rf /tmp/ws && mkdir -p /tmp/ws/notes
echo "hello notes" > /tmp/ws/notes/todo.txt
caos put /tmp/ws /cas/ws >/dev/null || fail "publishing the workspace"
ws=$(caos hash /cas/ws)

SUBAGENT_PROMPT="inspect the snapshot and report the notes file"
SUBAGENT_DONE_TEXT="subagent round complete"
rm -rf /tmp/stub && mkdir -p /tmp/stub
printf '{"content":[{"id":"toolu_spawn","input":{"prompt":"%s"},"name":"spawn_agent","type":"tool_use"}],"stop_reason":"tool_use"}' \
  "$SUBAGENT_PROMPT" > /tmp/stub/response-1.json
# Parent and child race for the next two API slots. Both responses are terminal
# and equivalent, so FIFO release order does not couple the test to scheduling.
mkfifo /tmp/stub/response-2.json /tmp/stub/response-3.json
stub_pid=""
port=""
start_stub /tmp/stub stub_pid port

new_llm_conversation llm-subagent "$port" "$ws" \
  "You are a coding agent operating on a git workspace."

stage "spawn a durable child conversation"
LLM_TEST_USERNAME=Alice
dispatch_turn "$ws" "delegate a focused check"
user1=$human
request1=$request

for request_number in 2 3; do
  request_seen=0
  for _ in $(seq 1 300); do
    if [ -e "/tmp/stub/request-$request_number.json" ]; then request_seen=1; break; fi
    sleep 0.2
  done
  [ "$request_seen" -eq 1 ] || fail "subagent API request $request_number never arrived"
  printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
    "$SUBAGENT_DONE_TEXT" > "/tmp/stub/response-$request_number.json"
done

head1=$(wait_turn) || {
  echo "--- stub log" >&2; cat /tmp/stub/log >&2 || true
  fail "the parent turn never reached a terminal event"
}
events1=$(git log --first-parent --format=%B "$user1..$head1")
spawn_detail=$(jq -r \
  'select(.result.tool_use_id == "toolu_spawn") | .result.content[0].text' \
  <<<"$events1")
[ -n "$spawn_detail" ] || fail "spawn_agent result was not recorded"
agent=$(jq -r '.agent // empty' <<<"$spawn_detail")
agent_task=$(jq -r '.task // empty' <<<"$spawn_detail")
agent_request=$(jq -r '.request // empty' <<<"$spawn_detail")
[[ "$agent" = agent-* ]] || fail "spawn_agent returned invalid child id: $agent"
assert_oid "$agent_task" "subagent task"
assert_oid "$agent_request" "subagent request"

# The child is INDEPENDENT of the parent turn, so its completion appends after
# it — polled on the parent's spine, exactly as the async task does.
agent_result=""
for _ in $(seq 1 300); do
  head1=$(current_head)
  events1=$(git log --first-parent --format=%B "$user1..$head1")
  agent_result=$(jq -r --arg task "$agent_task" \
    'select(.async.task == $task and (.async.status == "complete" or .async.status == "failed")) | .async.result // empty' \
    <<<"$events1")
  if [ -n "$agent_result" ]; then break; fi
  sleep 0.2
done
assert_oid "$agent_result" "subagent result"
agent_ref="refs/caos/v2/conversations/$agent/head"
agent_head=$(remote_tip "$agent_ref") || fail "subagent has no canonical head"
[ "$agent_head" = "$agent_result" ] || fail "subagent result is not its canonical head"
git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$agent_head" \
  || fail "fetching subagent head"

stage "child identity, ownership, and inherited workspace"
agent_key=$(printf '%s' "$agent" | od -An -tx1 | tr -d ' \n')
active_ref="refs/caos/v2/users/u-416c696365/conversations/active/c-$agent_key"
[ -n "$(remote_tip "$active_ref")" ] \
  || fail "subagent is absent from its human owner's active index"
title_ref="refs/caos/v2/conversations/$agent/title"
title=$(remote_tip "$title_ref")
[ -n "$title" ] || fail "subagent has no title ref"
git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$title" \
  || fail "fetching subagent title"
[ "$(git cat-file blob "$title")" = "$SUBAGENT_PROMPT" ] || fail "subagent title is wrong"
grep -qF "$SUBAGENT_DONE_TEXT" <<<"$(git show -s --format=%B "$agent_head")" \
  || fail "subagent terminal report is missing"
[ "$(git show "$agent_head:notes/todo.txt")" = "hello notes" ] \
  || fail "subagent did not inherit the clean workspace snapshot"
if git cat-file -e "$agent_head:.caos" 2>/dev/null; then
  fail "subagent inherited parent harness state"
fi
agent_events=$(git log --first-parent --format=%B "$agent_head")
grep -qF "\"conversation\":\"$conv\"" <<<"$agent_events" \
  || fail "subagent root lacks its durable parent conversation"
grep -qF "\"request\":\"$request1\"" <<<"$agent_events" \
  || fail "subagent root lacks its durable parent request"
grep -qF '"call":"toolu_spawn"' <<<"$agent_events" \
  || fail "subagent root lacks its durable parent call"
grep -qF '"username":"Alice"' <<<"$agent_events" \
  || fail "subagent root lacks its human owner"

stage "done"
printf 'llm-subagent: ALL PASS\n' > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
