#!/usr/bin/env bash
# Durable independent work through llm-step: the primary turn queues a request
# and becomes idle, completion appends later, and the following turn receives a
# deterministic completion notice.
set -euo pipefail
# The dependency is mounted only inside the test wrapper and exports globals.
# shellcheck disable=SC1091
source DEEP-DEPS/llm-test/common.sh

stage "workspace, worker barrier, and scripted model"
llm_test_setup
stub_host="${stub_host:?llm_test_setup did not set stub_host}"
mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
echo "You are a coding agent operating on a git workspace." > system.txt
git add -A
gc commit -qm fixtures
base=$(mkcommit "HEAD:ws" base)

# The independent worker reaches this server and blocks on its response FIFO.
# Unlike the old combined test, both stub servers use the readiness probe; no
# fixed half-second sleep sits on the test's critical path.
mkdir async-gate
mkfifo async-gate/response-1.json
gate_pid=""
gate_port=""
start_stub async-gate gate_pid gate_port
gate_pid="${gate_pid:?start_stub did not set gate_pid}"
async_request=$("$CAOS_CLI" prepare-request --base:@=DEEP-DEPS/bash \
  --worker1:@=test/async.sh --gate-host="$stub_host" --gate-port="$gate_port")
assert_oid "$async_request" "independent subrequest"

ASYNC_QUEUED_TEXT="the independent task is queued"
ASYNC_OBSERVED_TEXT="I observed the independent task completion"
mkdir stub
printf '{"content":[{"id":"toolu_async","input":{"request":"%s"},"name":"run_async","type":"tool_use"}],"stop_reason":"tool_use"}' \
  "$async_request" > stub/response-1.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$ASYNC_QUEUED_TEXT" > stub/response-2.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$ASYNC_OBSERVED_TEXT" > stub/response-3.json
stub_pid=""
port=""
start_stub stub stub_pid port
stub_pid="${stub_pid:?start_stub did not set stub_pid}"
new_llm_conversation llm-async "$port"
conv="${conv:?new_llm_conversation did not set conv}"
conversation_ref="${conversation_ref:?new_llm_conversation did not set conversation_ref}"
llm="${llm:?new_llm_conversation did not set llm}"

stage "primary turn queues work and becomes idle"
user1=$(mkcommit "HEAD:ws" \
  "{\"base\":\"$base\",\"author\":\"user\",\"content\":\"queue the independent request\"}" \
  "$base")
request1=$("$CAOS_CLI" prepare-request --base:hash="$llm" --head:commit="$user1")
assert_oid "$request1" "primary prepared request"
admitted1=$(mkcommit "HEAD:ws" \
  "{\"request\":\"$request1\",\"request_head\":\"$user1\",\"status\":\"queued\"}" \
  "$user1")
git push --quiet caos "$admitted1:$conversation_ref" \
  || fail "publishing independent-work admission"
"$CAOS_CLI" run --base:hash="$request1" >/tmp/llm-async-result \
  || fail "running independent-work turn"

head1=$(fetch_head)
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
grep -qF '"name":"run_async"' stub/request-1.json \
  || fail "run_async was not registered for the model"
grep -qF '"tool_use_id":"toolu_async"' stub/request-2.json \
  || fail "run_async's immediate result was not replayed"
grep -qF "$task" stub/request-2.json \
  || fail "run_async's immediate result omitted the task"

gate_reached=0
for _ in $(seq 1 300); do
  if [ -e async-gate/request-1.json ]; then gate_reached=1; break; fi
  sleep 0.2
done
[ "$gate_reached" -eq 1 ] || fail "independent worker never reached its barrier"
[ "$(remote_tip "$conversation_ref")" = "$head1" ] \
  || fail "conversation advanced while independent work was blocked"
[ -z "$(remote_exact_ref "refs/caos/res/$task" 2>/dev/null || true)" ] \
  || fail "independent task published a result before release"

stage "completion appends and is observed next turn"
printf '%s\n' '{"content":[],"stop_reason":"end_turn"}' > async-gate/response-1.json
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

task_result=""
for _ in $(seq 1 300); do
  task_result=$(remote_exact_ref "refs/caos/res/$task" 2>/dev/null || true)
  if [ -n "$task_result" ]; then break; fi
  sleep 0.2
done
[ -n "$task_result" ] || fail "independent task has no result ref"

tree1=$(git rev-parse "$completion_head^{tree}")
user2=$(mkcommit "$tree1" \
  '{"author":"user","content":"what completed?"}' "$completion_head")
request2=$("$CAOS_CLI" prepare-request --base:hash="$llm" --head:commit="$user2")
admitted2=$(mkcommit "$tree1" \
  "{\"request\":\"$request2\",\"request_head\":\"$user2\",\"status\":\"queued\"}" \
  "$user2")
git push --quiet caos "$admitted2:$conversation_ref" \
  || fail "publishing post-completion admission"
"$CAOS_CLI" run --base:hash="$request2" >/tmp/llm-async-result-2 \
  || fail "running post-completion turn"

head2=$(fetch_head)
grep -qF "$ASYNC_OBSERVED_TEXT" <<<"$(git show -s --format=%B "$head2")" \
  || fail "post-completion turn did not finish"
notice="Independent task $task is complete. Its result is addressed by that task hash."
grep -qF "$notice" stub/request-3.json \
  || fail "later model step did not observe independent completion"
[ "$(git rev-parse "$head2^{tree}")" = "$tree1" ] \
  || fail "post-completion observation changed the workspace"

stage "done"
echo "llm-async: ALL PASS" >&2
