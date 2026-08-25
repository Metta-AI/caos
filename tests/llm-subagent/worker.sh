#!/usr/bin/env bash
# Subagents are ordinary durable conversations: spawning returns stable
# identifiers, and the child inherits a clean workspace and human owner.
set -euo pipefail
# The dependency is mounted only inside the test wrapper and exports globals.
# shellcheck disable=SC1091
source DEEP-DEPS/llm-test/common.sh

stage "workspace and scripted parent/child model"
llm_test_setup
stub_host="${stub_host:?llm_test_setup did not set stub_host}"
mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
echo "You are a coding agent operating on a git workspace." > system.txt
git add -A
gc commit -qm fixtures
base=$(mkcommit "HEAD:ws" base)

SUBAGENT_PROMPT="inspect the snapshot and report the notes file"
SUBAGENT_DONE_TEXT="subagent round complete"
mkdir stub
printf '{"content":[{"id":"toolu_spawn","input":{"prompt":"%s"},"name":"spawn_agent","type":"tool_use"}],"stop_reason":"tool_use"}' \
  "$SUBAGENT_PROMPT" > stub/response-1.json
# Parent and child race for the next two API slots. Both responses are terminal
# and equivalent, so FIFO release order does not couple the test to scheduling.
mkfifo stub/response-2.json stub/response-3.json
stub_pid=""
port=""
start_stub stub stub_pid port
stub_pid="${stub_pid:?start_stub did not set stub_pid}"
new_llm_conversation llm-subagent "$port"
conv="${conv:?new_llm_conversation did not set conv}"
conversation_ref="${conversation_ref:?new_llm_conversation did not set conversation_ref}"
llm="${llm:?new_llm_conversation did not set llm}"

stage "spawn a durable child conversation"
user1=$(mkcommit "HEAD:ws" \
  "{\"base\":\"$base\",\"author\":\"user\",\"username\":\"Alice\",\"content\":\"delegate a focused check\"}" \
  "$base")
request1=$("$CAOS_CLI" prepare-request --base:hash="$llm" --head:commit="$user1")
assert_oid "$request1" "subagent prepared request"
admitted1=$(mkcommit "HEAD:ws" \
  "{\"request\":\"$request1\",\"request_head\":\"$user1\",\"status\":\"queued\"}" \
  "$user1")
git push --quiet caos "$admitted1:$conversation_ref" \
  || fail "publishing subagent admission"
"$CAOS_CLI" run --base:hash="$request1" >/tmp/llm-subagent-result 2>/tmp/llm-subagent-error &
spawn_pid=$!
LLM_TEST_PIDS+=("$spawn_pid")

for request_number in 2 3; do
  request_seen=0
  for _ in $(seq 1 300); do
    if [ -e "stub/request-$request_number.json" ]; then request_seen=1; break; fi
    # The primary turn may finish before the independent child reaches the
    # model. Its process ending therefore says nothing about child progress.
    sleep 0.2
  done
  [ "$request_seen" -eq 1 ] || fail "subagent API request $request_number never arrived"
  printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
    "$SUBAGENT_DONE_TEXT" > "stub/response-$request_number.json"
done
if ! wait "$spawn_pid"; then
  fail "running subagent turn: $(cat /tmp/llm-subagent-error)"
fi

head1=$(fetch_head)
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

agent_result=""
for _ in $(seq 1 300); do
  head1=$(fetch_head)
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
echo "llm-subagent: ALL PASS" >&2
