#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "workspace, worker barrier, and scripted model"
llm_test_setup
caos get /cas/args/async || fail "reading async.sh"

rm -rf /tmp/ws
mkdir -p /tmp/ws/notes
echo "hello notes" > /tmp/ws/notes/todo.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the workspace")

rm -rf /tmp/async-gate
mkdir -p /tmp/async-gate
mkfifo /tmp/async-gate/response-1.json
gate_pid=""
gate_port=""
start_stub /tmp/async-gate gate_pid gate_port
async_request=$(caos prepare-request --base:hash="$(caos hash /cas/args/bash)" \
  --worker1:@=/cas/args/async --gate-host="$stub_host" --gate-port="$gate_port") \
  || fail "preparing the independent subrequest"
assert_oid "$async_request" "independent subrequest"

ASYNC_QUEUED_TEXT="the independent task is queued"
ASYNC_OBSERVED_TEXT="I observed the independent task completion"
mkdir -p /tmp/stub
printf '{"content":[{"id":"toolu_async","input":{"request":"%s"},"name":"run_async","type":"tool_use"}],"stop_reason":"tool_use"}' \
  "$async_request" > /tmp/stub/response-1.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$ASYNC_QUEUED_TEXT" > /tmp/stub/response-2.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$ASYNC_OBSERVED_TEXT" > /tmp/stub/response-3.json
start_stub /tmp/stub

new_llm_conversation llm-async "$STUB_PORT" "$ws" \
  "You are a coding agent operating on a git workspace."

stage "primary turn records pending work and becomes idle"
dispatch_turn "queue the independent request"
wait_turn || fail "the primary turn never reached a terminal head"
head1=$head
workspace1=$(workspace_commit "$head1")
$TOOL async --repo /tmp/repo --head "$head1" > /tmp/async.pending
task=$(jq -r 'select(.status == "pending") | .task' /tmp/async.pending)
assert_oid "$task" "pending task"
[ "$(grep -c '"status":"pending"' /tmp/async.pending)" -eq 1 ] \
  || fail "run_async did not record exactly one pending task"
grep -qF '"name":"run_async"' /tmp/stub/request-1.json \
  || fail "run_async was not registered for the model"
grep -qF '"tool_use_id":"toolu_async"' /tmp/stub/request-2.json \
  || fail "run_async's immediate result was not replayed"
grep -qF "$task" /tmp/stub/request-2.json \
  || fail "run_async's immediate result omitted the task"

wait_for_file /tmp/async-gate/request-1.json \
  || fail "independent worker never reached its barrier"
[ "$(remote_tip "$conversation_ref")" = "$head1" ] \
  || fail "conversation advanced while independent work was blocked"

stage "completion changes only the async record"
printf '%s\n' '{"content":[],"stop_reason":"end_turn"}' > /tmp/async-gate/response-1.json
completion_head=""
seen=$head1
for _ in $(seq 1 300); do
  candidate=$(remote_tip "$conversation_ref")
  if [ "$candidate" != "$seen" ]; then
    seen=$candidate
    if $TOOL async --repo /tmp/repo --head "$candidate" --task "$task" \
      | jq -e '.status == "complete"' >/dev/null 2>&1; then
      completion_head=$candidate
      break
    fi
  fi
  sleep 0.2
done
[ -n "$completion_head" ] || fail "independent completion was not appended"
[ "$(git rev-parse "$completion_head^1")" = "$head1" ] \
  || fail "completion did not append to the idle head"
changed=$(git diff-tree --no-commit-id --name-only -r "$head1" "$completion_head")
[ "$changed" = ".caos/async/$task.json" ] \
  || fail "completion changed more than its async record: $changed"
[ "$(workspace_commit "$completion_head")" = "$workspace1" ] \
  || fail "completion changed main"
$TOOL async --repo /tmp/repo --head "$completion_head" --task "$task" > /tmp/async.complete
task_result=$(jq -r 'select(.status == "complete") | .result' /tmp/async.complete)
assert_oid "$task_result" "independent task result"

stage "the next turn receives a durable system notice"
head=$completion_head
dispatch_turn "what completed?"
wait_turn || fail "the post-completion turn never reached a terminal head"
head2=$head
notice="Independent task $task is complete. Its result is $task_result."
grep -qF "$notice" /tmp/stub/request-3.json \
  || fail "later model step did not observe independent completion"
$TOOL transcript --repo /tmp/repo --head "$head2" > /tmp/async.transcript
system_notice=0
while read -r _ role _ _ encoded; do
  if [ "$role" = system ] && [ "$(printf '%s' "$encoded" | jq -r .)" = "$notice" ]; then
    system_notice=1
  fi
done < /tmp/async.transcript
[ "$system_notice" -eq 1 ] || fail "completion notice is not a system transcript entry"
[ "$(workspace_commit "$head2")" = "$workspace1" ] \
  || fail "post-completion observation changed main"

pass llm-async
