#!/bin/bash
# tests/llm-async — a WORKER test, in dev/worker-test (it needs git).
#
# Durable independent work through llm-step: the primary turn queues a request
# and becomes idle, completion appends later, and the following turn receives a
# deterministic completion notice.
#
# THE BARRIER IS A SECOND STUB WITH A FIFO RESPONSE. async.sh POSTs to it and
# blocks reading the reply, so "the conversation went idle while the work was
# still running" is a fact this test controls rather than races against.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "workspace, worker barrier, and scripted model"
llm_test_setup
caos get -r /cas/args/bash || fail "reading the bash image"
caos get /cas/args/async  || fail "reading async.sh"

rm -rf /tmp/ws && mkdir -p /tmp/ws/notes
echo "hello notes" > /tmp/ws/notes/todo.txt
caos put /tmp/ws /cas/ws >/dev/null || fail "publishing the workspace"
ws=$(caos hash /cas/ws)

# The independent worker reaches this server and blocks on its response FIFO.
rm -rf /tmp/async-gate && mkdir -p /tmp/async-gate
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
stub_pid=""
port=""
start_stub /tmp/stub stub_pid port

new_llm_conversation llm-async "$port" "$ws" \
  "You are a coding agent operating on a git workspace."

stage "primary turn queues work and becomes idle"
dispatch_turn "$ws" "queue the independent request"
user1=$human
head1=$(wait_turn) || {
  echo "--- stub log" >&2; cat /tmp/stub/log >&2 || true
  fail "the primary turn never reached a terminal event"
}
terminal1=$(git show -s --format=%B "$head1")
grep -qF '"status":"idle"' <<<"$terminal1" \
  || fail "primary turn did not become idle after run_async"
grep -qF "$ASYNC_QUEUED_TEXT" <<<"$terminal1" \
  || fail "primary turn did not finish with its scripted response"
events1=$(git log --first-parent --format=%B "$user1..$head1")
mapfile -t pending_tasks < <(
  jq -r 'select(.async.status == "pending") | .async.task' <<<"$events1"
)
[ "${#pending_tasks[@]}" -eq 1 ] || fail "run_async did not record exactly one task"
task=${pending_tasks[0]}
assert_oid "$task" "pending task"
grep -qF '"name":"run_async"' /tmp/stub/request-1.json \
  || fail "run_async was not registered for the model"
grep -qF '"tool_use_id":"toolu_async"' /tmp/stub/request-2.json \
  || fail "run_async's immediate result was not replayed"
grep -qF "$task" /tmp/stub/request-2.json \
  || fail "run_async's immediate result omitted the task"

gate_reached=0
for _ in $(seq 1 300); do
  if [ -e /tmp/async-gate/request-1.json ]; then gate_reached=1; break; fi
  sleep 0.2
done
[ "$gate_reached" -eq 1 ] || fail "independent worker never reached its barrier"
[ "$(remote_tip "$conversation_ref")" = "$head1" ] \
  || fail "conversation advanced while independent work was blocked"

stage "completion appends and is observed next turn"
printf '%s\n' '{"content":[],"stop_reason":"end_turn"}' > /tmp/async-gate/response-1.json
completion_head=""
for _ in $(seq 1 300); do
  candidate=$(remote_tip "$conversation_ref")
  if [ "$candidate" != "$head1" ]; then
    git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$candidate" \
      || fail "fetching independent completion"
    if [ "$(git show -s --format=%B "$candidate" | jq -r '.async.status // empty')" = complete ]; then
      completion_head=$candidate
      break
    fi
  fi
  sleep 0.2
done
[ -n "$completion_head" ] || fail "independent completion was not appended"
[ "$(git rev-parse "$completion_head^1")" = "$head1" ] \
  || fail "completion did not append after the primary turn"
[ "$(git rev-parse "$completion_head^{tree}")" = "$(git rev-parse "$head1^{tree}")" ] \
  || fail "completion changed the workspace"

completion_event=$(git show -s --format=%B "$completion_head")
task_result=$(jq -r --arg task "$task" \
  'select(.async.task == $task and .async.status == "complete") | .async.result // empty' \
  <<<"$completion_event")
assert_oid "$task_result" "independent task result"

tree1=$(git rev-parse "$completion_head^{tree}")
dispatch_turn "$tree1" "what completed?" "$completion_head"
head2=$(wait_turn) || fail "the post-completion turn never reached a terminal event"
grep -qF "$ASYNC_OBSERVED_TEXT" <<<"$(git show -s --format=%B "$head2")" \
  || fail "post-completion turn did not finish"
notice="Independent task $task is complete. Its result is $task_result."
grep -qF "$notice" /tmp/stub/request-3.json \
  || fail "later model step did not observe independent completion"
[ "$(git rev-parse "$head2^{tree}")" = "$tree1" ] \
  || fail "post-completion observation changed the workspace"

stage "done"
printf 'llm-async: ALL PASS\n' > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
