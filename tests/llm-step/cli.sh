#!/usr/bin/env bash
# End-to-end conversation test with a scripted LLM. The worker owns the
# canonical head after dispatch; its result object is deliberately ignored.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
gc() { git -c user.email=test@caos -c user.name=caos "$@"; }
mkcommit() { # <tree> <message> [parent] -> commit
  local tree=$1 message=$2 parent=${3:-}
  local parents=()
  if [ -n "$parent" ]; then parents=(-p "$parent"); fi
  gc commit-tree "$tree" "${parents[@]}" -m "$message"
}
remote_head() {
  local advertised
  advertised=$(git ls-remote --refs caos "$conversation_ref") \
    || fail "reading canonical conversation head"
  printf '%s\n' "${advertised%%$'\t'*}"
}
remote_exact_ref() { # <ref>
  curl -fsS -X POST -H 'content-type: application/json' \
    --data "{\"ref\":\"$1\"}" "$CAOS_SERVER_URL/ref/read"
}
fetch_head() {
  local head
  head=$(remote_head)
  [ -n "$head" ] || fail "canonical conversation head is absent"
  git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$head" \
    || fail "fetching canonical conversation head"
  printf '%s\n' "$head"
}
assert_event_spine() { # <head> <stop>
  local current=$1 stop=$2 count=0 message parent declared_base roots=0
  while [ "$current" != "$stop" ]; do
    message=$(git show -s --format=%B "$current")
    jq -e 'type == "object" and (has("v") | not)' <<<"$message" >/dev/null \
      || fail "invalid event on conversation spine: $current"
    parent=$(git rev-parse "$current^1")
    declared_base=$(jq -r '.base // empty' <<<"$message")
    if [ -n "$declared_base" ]; then
      [ "$declared_base" = "$parent" ] \
        || fail "root event $current does not name its first parent as base"
      [ "$declared_base" = "$stop" ] \
        || fail "root event $current names the wrong conversation base"
      roots=$((roots + 1))
    elif [ "$parent" = "$stop" ]; then
      fail "oldest event $current has no explicit base"
    fi
    current=$parent
    count=$((count + 1))
  done
  [ "$roots" -eq 1 ] || fail "event spine did not contain exactly one explicit base"
  [ "$count" -ge 2 ] || fail "event spine is unexpectedly short"
}

echo "== workspace and scripted model ==" >&2
"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry \
  || fail "resolving llm-stub"
stub_bin=/tmp/llm-stub-bin
install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"

mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
printf '#!/bin/sh\necho hi\n' > ws/run.sh
chmod +x ws/run.sh
echo "You are a coding agent operating on a git workspace." > system.txt
git add -A
gc commit -qm fixtures
base=$(mkcommit "HEAD:ws" base)

# A second scripted server is a deterministic barrier for the independent
# worker. It records the worker's request, then blocks on its response FIFO
# until this test releases it after the primary turn has become idle.
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
stub_pid=""
gate_pid=""
interrupt_pid=""
cleanup() {
  if [ -n "$stub_pid" ]; then
    kill "$stub_pid" 2>/dev/null || true
  fi
  if [ -n "$gate_pid" ]; then
    kill "$gate_pid" 2>/dev/null || true
  fi
  if [ -n "$interrupt_pid" ]; then
    kill "$interrupt_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

mkdir async-gate
mkfifo async-gate/response-1.json
for _ in 1 2 3 4 5; do
  gate_port=$((20000 + RANDOM % 20000))
  "$stub_bin" "0.0.0.0:$gate_port" "$PWD/async-gate" 2>async-gate/log &
  gate_pid=$!
  sleep 0.5
  if kill -0 "$gate_pid" 2>/dev/null; then break; fi
  gate_pid=""
done
[ -n "$gate_pid" ] || fail "could not start async gate: $(cat async-gate/log)"

async_request=$("$CAOS_CLI" prepare-request DEEP-DEPS/bash -- \
  --worker1:@=test/async.sh --gate-host="$stub_host" --gate-port="$gate_port")
[ "${#async_request}" -eq 40 ] && [[ "$async_request" =~ ^[0-9a-f]+$ ]] \
  || fail "independent subrequest is not exact R: $async_request"

R1='[{"signature":"sig-abc","thinking":"I should create the file.","type":"thinking"},{"text":"Creating out.txt.","type":"text"},{"id":"toolu_01","input":{"cmd":"echo hi > out.txt","paths":[]},"name":"bash","type":"tool_use"}]'
R2='[{"id":"toolu_02","input":{"cmd":"cat out.txt","paths":["out.txt"]},"name":"bash","type":"tool_use"},{"id":"toolu_03","input":{"cmd":"echo boom >&2; exit 3","paths":[]},"name":"bash","type":"tool_use"}]'
EARLY_INTERJECTION_TEXT="also preserve the executable bit"
INTERJECTION_TEXT="one more thing before you finish"
STALE_T2_TEXT="the workspace still holds out.txt"
T2_TEXT="yes, I also saw your last message"
ASYNC_QUEUED_TEXT="the independent task is queued"
ASYNC_OBSERVED_TEXT="I observed the independent task completion"
SUBAGENT_PROMPT="inspect the snapshot and report the notes file"
SUBAGENT_DONE_TEXT="subagent round complete"
RESULT_DONE_TEXT="I inspected the subagent result"
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1" > stub/response-1.json
printf '{"content":%s,"stop_reason":"tool_use"}' "$R2" > stub/response-2.json
printf '{"content":[{"text":"done: out.txt contains hi","type":"text"}],"stop_reason":"end_turn"}' \
  > stub/response-3.json
# Block the second turn's apparently-terminal response after the stub records
# its request. The test appends an interjection at that exact boundary, then
# verifies llm-step discards the stale terminal state and answers again.
mkfifo stub/response-4.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' "$T2_TEXT" \
  > stub/response-5.json
printf '{"content":[{"id":"toolu_async","input":{"request":"%s"},"name":"run_async","type":"tool_use"}],"stop_reason":"tool_use"}' \
  "$async_request" > stub/response-6.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$ASYNC_QUEUED_TEXT" > stub/response-7.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$ASYNC_OBSERVED_TEXT" > stub/response-8.json
printf '{"content":[{"id":"toolu_spawn","input":{"prompt":"%s"},"name":"spawn_agent","type":"tool_use"}],"stop_reason":"tool_use"}' \
  "$SUBAGENT_PROMPT" > stub/response-9.json
# The parent and child race for the next API slot. Both terminal responses are
# equivalent, while FIFOs let the test release each request in observed order.
mkfifo stub/response-10.json stub/response-11.json
mkfifo stub/response-15.json

for _ in 1 2 3 4 5; do
  port=$((20000 + RANDOM % 20000))
  "$stub_bin" "0.0.0.0:$port" "$PWD/stub" 2>stub/log &
  stub_pid=$!
  ready=0
  for _ in {1..400}; do
    if ! kill -0 "$stub_pid" 2>/dev/null; then break; fi
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then ready=1; break; fi
    sleep 0.005
  done
  if [ "$ready" = 1 ]; then break; fi
  kill "$stub_pid" 2>/dev/null || true
  stub_pid=""
done
[ -n "$stub_pid" ] || fail "could not start llm-stub: $(cat stub/log)"

echo "== dispatch first turn ==" >&2
conv="llm-step-$(printf '%s' "${CAOS_SALT:-dev}" | tr -cd '0-9a-zA-Z')-$$"
conversation_ref="refs/caos/v2/conversations/$conv/head"
llm=$("$CAOS_CLI" curry DEEP-DEPS/llm-step -- \
  --api-key=test-key --system:@=system.txt \
  --model=test-model --base-url="http://$stub_host:$port" \
  --conversation="$conv")

user1=$(mkcommit "HEAD:ws" \
  "{\"base\":\"$base\",\"author\":\"user\",\"content\":\"create out.txt containing hi, then confirm\"}" \
  "$base")
request1=$("$CAOS_CLI" prepare-request "$llm" -- --head:commit="$user1")
[ "${#request1}" -eq 40 ] && [[ "$request1" =~ ^[0-9a-f]+$ ]] \
  || fail "first prepared request is not exact Q: $request1"
admitted1=$(mkcommit "HEAD:ws" \
  "{\"request\":\"$request1\",\"request_head\":\"$user1\",\"status\":\"queued\"}" \
  "$user1")
early_interjection=$(mkcommit "HEAD:ws" \
  "{\"author\":\"user\",\"content\":\"$EARLY_INTERJECTION_TEXT\",\"username\":\"racer\"}" \
  "$admitted1")
git push --quiet caos "$early_interjection:$conversation_ref" \
  || fail "publishing queued event and pre-start interjection"
"$CAOS_CLI" run "$request1" -- >/tmp/llm-step-result || fail "running first turn"
[ -n "$(remote_exact_ref "refs/caos/res/$request1")" ] \
  || fail "first exact request Q has no result ref"

head1=$(fetch_head)
assert_event_spine "$head1" "$base"

echo "== durable events and workspace ==" >&2
events1=$(git log --first-parent --format=%B "$base..$head1")
grep -qF "\"request\":\"$request1\"" <<<"$events1" \
  || fail "worker did not record the first running request"
grep -qF "$EARLY_INTERJECTION_TEXT" <<<"$events1" \
  || fail "interjection before the worker's running append was lost"
grep -Eq '"response"[[:space:]]*:[[:space:]]*\[' <<<"$events1" || fail "model response record is missing"
grep -Eq '"calls"[[:space:]]*:[[:space:]]*\[' <<<"$events1" || fail "ordered call record is missing"
grep -Eq '"result"[[:space:]]*:[[:space:]]*\{' <<<"$events1" || fail "tool result record is missing"
grep -qF '"id":"toolu_01"' <<<"$events1" || fail "model tool call was not recorded"
grep -qF '"tool_use_id":"toolu_01"' <<<"$events1" || fail "tool result was not recorded"
grep -qF '"tool_use_id":"toolu_03"' <<<"$events1" || fail "second-round result was not recorded"
grep -qF '"is_error":true' <<<"$events1" || fail "failed tool result lost its error bit"
grep -qF 'done: out.txt contains hi' <<<"$events1" || fail "assistant transcript was not recorded"
grep -qF '"model":"test-model"' <<<"$events1" || fail "assistant model was not recorded"
terminal1=$(git show -s --format=%B "$head1")
grep -Eq '"status"[[:space:]]*:[[:space:]]*"idle"' <<<"$terminal1" \
  || fail "turn did not become idle"
grep -qF "\"request\":\"$request1\"" <<<"$terminal1" \
  || fail "terminal event did not identify its request"
[ "$(git show "$head1:out.txt")" = hi ] || fail "out.txt missing from canonical head"
[ "$(git show "$head1:notes/todo.txt")" = "hello notes" ] || fail "untouched subtree lost"
[ "$(git ls-tree "$head1" run.sh | cut -d' ' -f1)" = 100755 ] \
  || fail "executable mode changed"

echo "== model replay and second turn ==" >&2
grep -qF '"max_tokens":64000' stub/request-1.json || fail "max_tokens not sent"
grep -qF "\"messages\":[{\"content\":\"create out.txt containing hi, then confirm\",\"role\":\"user\"},{\"content\":\"$EARLY_INTERJECTION_TEXT\",\"role\":\"user\"}]" \
  stub/request-1.json || fail "queued message and pre-start interjection were not both replayed"
grep -qF "\"content\":$R1,\"role\":\"assistant\"" stub/request-2.json \
  || fail "round-one response was not replayed verbatim"
grep -qF '"tool_use_id":"toolu_01","type":"tool_result"' stub/request-2.json \
  || fail "round-one result missing"
grep -qF "\"content\":$R2,\"role\":\"assistant\"" stub/request-3.json \
  || fail "round-two response was not replayed verbatim"
grep -qF 'exit: 3' stub/request-3.json || fail "failed command result missing"
[ ! -f stub/request-4.json ] || fail "unexpected extra model round"

tree1=$(git rev-parse "$head1^{tree}")
user2=$(mkcommit "$tree1" \
  '{"author":"user","content":"and now?"}' "$head1")
request2=$("$CAOS_CLI" prepare-request "$llm" -- --head:commit="$user2")
[ "${#request2}" -eq 40 ] && [[ "$request2" =~ ^[0-9a-f]+$ ]] \
  || fail "second prepared request is not exact Q: $request2"
admitted2=$(mkcommit "$tree1" \
  "{\"request\":\"$request2\",\"request_head\":\"$user2\",\"status\":\"queued\"}" \
  "$user2")
git push --quiet caos "$admitted2:$conversation_ref" || fail "publishing second request admission"
"$CAOS_CLI" run "$request2" -- >/tmp/llm-step-result-2 2>/tmp/llm-step-error-2 &
run2_pid=$!

request_started=0
for _ in $(seq 1 150); do
  if [ -e stub/request-4.json ]; then
    request_started=1
    break
  fi
  if ! kill -0 "$run2_pid" 2>/dev/null; then
    fail "second turn exited before reaching the blocked response: $(cat /tmp/llm-step-error-2)"
  fi
  sleep 0.2
done
[ "$request_started" -eq 1 ] || fail "second turn never reached the blocked response"

observed=$(fetch_head)
running2=$observed
[ "$(git rev-parse "$running2^1")" = "$admitted2" ] \
  || fail "worker running event is not immediately after the admission event"
running2_event=$(git show -s --format=%B "$running2")
grep -qF "\"request\":\"$request2\"" <<<"$running2_event" \
  || fail "worker did not record the second running request"
grep -qF '"status":"running"' <<<"$running2_event" \
  || fail "worker request event is not running"
interjection=$(mkcommit "$tree1" \
  "{\"author\":\"user\",\"content\":\"$INTERJECTION_TEXT\",\"username\":\"racer\"}" \
  "$observed")
git push --quiet --force-with-lease="$conversation_ref:$observed" \
  caos "$interjection:$conversation_ref" || fail "publishing terminal-race interjection"

printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$STALE_T2_TEXT" > stub/response-4.json
if ! wait "$run2_pid"; then
  fail "running interjected second turn: $(cat /tmp/llm-step-error-2)"
fi
[ -n "$(remote_exact_ref "refs/caos/res/$request2")" ] \
  || fail "second exact request Q has no result ref"

head2=$(fetch_head)
assert_event_spine "$head2" "$base"
[ "$(git rev-parse "$head2^{tree}")" = "$tree1" ] || fail "toolless turn changed the workspace"
[ "$(git rev-parse "$head2^1")" = "$interjection" ] \
  || fail "fresh terminal response is not immediately after the interjection"
[ "$(git rev-parse "$interjection^1")" = "$running2" ] \
  || fail "interjection is not immediately after the running event"
terminal2=$(git show -s --format=%B "$head2")
grep -Eq '"status"[[:space:]]*:[[:space:]]*"idle"' <<<"$terminal2" \
  || fail "second turn did not become idle"
grep -qF "\"request\":\"$request2\"" <<<"$terminal2" \
  || fail "second terminal event did not identify its request"
grep -qF "$T2_TEXT" <<<"$terminal2" || fail "interjection answer is not terminal"
events2=$(git log --first-parent --format=%B "$running2..$head2")
grep -qF "$INTERJECTION_TEXT" <<<"$events2" || fail "racing interjection was lost"
if grep -qF "$STALE_T2_TEXT" <<<"$events2"; then
  fail "stale pre-interjection terminal response became canonical"
fi
first_parent=$(git rev-list --first-parent "$head2")
interjection_count=$(grep -cxF "$interjection" <<<"$first_parent" || true)
[ "$interjection_count" -eq 1 ] || fail "interjection appears $interjection_count times on first parent"
grep -qF "\"content\":$R1,\"role\":\"assistant\"" stub/request-4.json \
  || fail "prior model response missing from second turn"
grep -qF "\"content\":$R2,\"role\":\"assistant\"" stub/request-4.json \
  || fail "prior second round missing from second turn"
grep -qF '{"content":"and now?","role":"user"}]' stub/request-4.json \
  || fail "second user message missing"
if grep -qF "$INTERJECTION_TEXT" stub/request-4.json; then
  fail "racing interjection leaked into the already-recorded model request"
fi
if grep -qF "$STALE_T2_TEXT" stub/request-5.json; then
  fail "discarded terminal response was replayed to the model"
fi
grep -qF "{\"content\":\"$INTERJECTION_TEXT\",\"role\":\"user\"}]" stub/request-5.json \
  || fail "racing interjection was not replayed in the replacement model call"

echo "== model-issued independent work ==" >&2
tree2=$(git rev-parse "$head2^{tree}")
user3=$(mkcommit "$tree2" \
  '{"author":"user","content":"queue the independent request"}' \
  "$head2")
request3=$("$CAOS_CLI" prepare-request "$llm" -- --head:commit="$user3")
[ "${#request3}" -eq 40 ] && [[ "$request3" =~ ^[0-9a-f]+$ ]] \
  || fail "third prepared request is not exact Q: $request3"
admitted3=$(mkcommit "$tree2" \
  "{\"request\":\"$request3\",\"request_head\":\"$user3\",\"status\":\"queued\"}" \
  "$user3")
git push --quiet caos "$admitted3:$conversation_ref" \
  || fail "publishing independent-work queued event"
"$CAOS_CLI" run "$request3" -- >/tmp/llm-step-result-3 \
  || fail "running independent-work turn"

# The model got an immediate tool result and ended the primary turn while the
# independent worker is still held at the separate server's FIFO.
head3=$(fetch_head)
terminal3=$(git show -s --format=%B "$head3")
grep -Eq '"status"[[:space:]]*:[[:space:]]*"idle"' <<<"$terminal3" \
  || fail "primary turn did not become idle after run_async"
grep -qF "$ASYNC_QUEUED_TEXT" <<<"$terminal3" \
  || fail "primary turn did not finish with its scripted response"
events3=$(git log --first-parent --format=%B "$user3..$head3")
grep -qF "\"request\":\"$request3\"" <<<"$events3" \
  || fail "worker did not record the independent-work running request"
mapfile -t pending_tasks < <(
  git log --first-parent --format=%B "$user3..$head3" \
    | jq -r 'select(.async.status == "pending") | .async.task'
)
[ "${#pending_tasks[@]}" -eq 1 ] || fail "run_async did not record exactly one task"
task=${pending_tasks[0]}
[ "${#task}" -eq 40 ] && [[ "$task" =~ ^[0-9a-f]+$ ]] \
  || fail "pending event has invalid task Q: $task"
grep -qF '"name":"run_async"' stub/request-6.json \
  || fail "run_async was not registered for the model"
grep -qF '"tool_use_id":"toolu_async"' stub/request-7.json \
  || fail "run_async's immediate result was not replayed"
grep -qF "$task" stub/request-7.json \
  || fail "run_async's immediate result omitted task Q"

gate_reached=0
for _ in $(seq 1 300); do
  if [ -e async-gate/request-1.json ]; then
    gate_reached=1
    break
  fi
  sleep 0.2
done
[ "$gate_reached" -eq 1 ] || fail "independent worker never reached its barrier"
[ "$(remote_head)" = "$head3" ] \
  || fail "conversation advanced while independent worker was blocked"
[ -z "$(remote_exact_ref "refs/caos/res/$task" 2>/dev/null || true)" ] \
  || fail "independent task published a result before release"

echo "== independent completion and later observation ==" >&2
printf '{"content":[],"stop_reason":"end_turn"}' > async-gate/response-1.json
completion_head=""
for _ in $(seq 1 300); do
  candidate=$(remote_head)
  if [ "$candidate" != "$head3" ]; then
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
[ "$(git rev-parse "$completion_head^1")" = "$head3" ] \
  || fail "independent completion did not append after the primary turn"
[ "$(git rev-parse "$completion_head^{tree}")" = "$(git rev-parse "$head3^{tree}")" ] \
  || fail "independent completion changed the workspace"

task_result=""
for _ in $(seq 1 300); do
  task_result=$(remote_exact_ref "refs/caos/res/$task" 2>/dev/null || true)
  if [ -n "$task_result" ]; then break; fi
  sleep 0.2
done
[ -n "$task_result" ] || fail "independent task Q has no result ref"

tree3=$(git rev-parse "$completion_head^{tree}")
user4=$(mkcommit "$tree3" \
  '{"author":"user","content":"what completed?"}' \
  "$completion_head")
request4=$("$CAOS_CLI" prepare-request "$llm" -- --head:commit="$user4")
admitted4=$(mkcommit "$tree3" \
  "{\"request\":\"$request4\",\"request_head\":\"$user4\",\"status\":\"queued\"}" \
  "$user4")
git push --quiet caos "$admitted4:$conversation_ref" \
  || fail "publishing post-completion queued event"
"$CAOS_CLI" run "$request4" -- >/tmp/llm-step-result-4 \
  || fail "running post-completion turn"

head4=$(fetch_head)
terminal4=$(git show -s --format=%B "$head4")
grep -qF "$ASYNC_OBSERVED_TEXT" <<<"$terminal4" \
  || fail "post-completion turn did not finish"
notice="Independent task $task is complete. Its result is addressed by that task hash."
grep -qF "$notice" stub/request-8.json \
  || fail "later model step did not observe independent completion"
[ "$(git rev-parse "$head4^{tree}")" = "$tree3" ] \
  || fail "post-completion observation changed the workspace"

echo "== durable subagent and explicit result read ==" >&2
tree4=$(git rev-parse "$head4^{tree}")
user5=$(mkcommit "$tree4" \
  '{"author":"user","username":"Alice","content":"delegate a focused check"}' \
  "$head4")
request5=$("$CAOS_CLI" prepare-request "$llm" -- --head:commit="$user5")
admitted5=$(mkcommit "$tree4" \
  "{\"request\":\"$request5\",\"request_head\":\"$user5\",\"status\":\"queued\"}" \
  "$user5")
git push --quiet caos "$admitted5:$conversation_ref" \
  || fail "publishing subagent queued event"
"$CAOS_CLI" run "$request5" -- >/tmp/llm-step-result-5 2>/tmp/llm-step-error-5 &
spawn_pid=$!

for request_number in 10 11; do
  request_seen=0
  for _ in $(seq 1 300); do
    if [ -e "stub/request-$request_number.json" ]; then
      request_seen=1
      break
    fi
    sleep 0.2
  done
  [ "$request_seen" -eq 1 ] || fail "subagent API request $request_number never arrived"
  printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
    "$SUBAGENT_DONE_TEXT" > "stub/response-$request_number.json"
done
if ! wait "$spawn_pid"; then
  fail "running subagent turn: $(cat /tmp/llm-step-error-5)"
fi

head5=$(fetch_head)
events5=$(git log --first-parent --format=%B "$user5..$head5")
spawn_detail=$(jq -r \
  'select(.result.tool_use_id == "toolu_spawn") | .result.content[0].text' \
  <<<"$events5")
[ -n "$spawn_detail" ] || fail "spawn_agent result was not recorded"
agent=$(jq -r '.agent // empty' <<<"$spawn_detail")
agent_task=$(jq -r '.task // empty' <<<"$spawn_detail")
agent_request=$(jq -r '.request // empty' <<<"$spawn_detail")
[[ "$agent" = agent-* ]] || fail "spawn_agent returned invalid child id: $agent"
[ "${#agent_task}" -eq 40 ] && [[ "$agent_task" =~ ^[0-9a-f]+$ ]] \
  || fail "spawn_agent returned invalid task: $agent_task"
[ "${#agent_request}" -eq 40 ] && [[ "$agent_request" =~ ^[0-9a-f]+$ ]] \
  || fail "spawn_agent returned invalid request: $agent_request"

agent_result=""
for _ in $(seq 1 300); do
  agent_result=$(remote_exact_ref "refs/caos/res/$agent_task" 2>/dev/null || true)
  if [ -n "$agent_result" ]; then break; fi
  sleep 0.2
done
[ -n "$agent_result" ] || fail "subagent task has no result ref"
agent_ref="refs/caos/v2/conversations/$agent/head"
agent_head=$(git ls-remote --refs caos "$agent_ref")
agent_head=${agent_head%%$'\t'*}
[ "$agent_head" = "$agent_result" ] || fail "subagent result is not its canonical head"
git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$agent_head" \
  || fail "fetching subagent head"
agent_key=$(printf '%s' "$agent" | od -An -tx1 | tr -d ' \n')
active_ref="refs/caos/v2/users/u-416c696365/conversations/active/c-$agent_key"
[ -n "$(remote_exact_ref "$active_ref")" ] \
  || fail "subagent is absent from its human owner's active index"
title_ref="refs/caos/v2/conversations/$agent/title"
title=$(remote_exact_ref "$title_ref")
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
grep -qF "\"request\":\"$request5\"" <<<"$agent_events" \
  || fail "subagent root lacks its durable parent request"
grep -qF '"call":"toolu_spawn"' <<<"$agent_events" \
  || fail "subagent root lacks its durable parent call"
grep -qF '"username":"Alice"' <<<"$agent_events" \
  || fail "subagent root lacks its human owner"

printf '{"content":[{"id":"toolu_result","input":{"request":"%s"},"name":"run_async","type":"tool_use"}],"stop_reason":"tool_use"}' \
  "$agent_request" > stub/response-12.json
printf '{"content":[{"id":"toolu_merge","input":{"theirs":"%s"},"name":"merge","type":"tool_use"}],"stop_reason":"tool_use"}' \
  "$agent_result" > stub/response-13.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$RESULT_DONE_TEXT" > stub/response-14.json
completion5=$(remote_head)
git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$completion5" \
  || fail "fetching subagent completion event"
tree5=$(git rev-parse "$completion5^{tree}")
user6=$(mkcommit "$tree5" \
  '{"author":"user","content":"inspect the delegated result"}' \
  "$completion5")
request6=$("$CAOS_CLI" prepare-request "$llm" -- --head:commit="$user6")
admitted6=$(mkcommit "$tree5" \
  "{\"request\":\"$request6\",\"request_head\":\"$user6\",\"status\":\"queued\"}" \
  "$user6")
git push --quiet caos "$admitted6:$conversation_ref" \
  || fail "publishing result-inspection queued event"
"$CAOS_CLI" run "$request6" -- >/tmp/llm-step-result-6 \
  || fail "running result-inspection turn"

head6=$(fetch_head)
grep -qF "$RESULT_DONE_TEXT" <<<"$(git show -s --format=%B "$head6")" \
  || fail "result-inspection turn did not finish"
grep -qF "$agent_result" stub/request-13.json \
  || fail "run_async did not return the child workspace commit"
[ "$(git rev-parse "$head6^{tree}")" = "$tree5" ] \
  || fail "merging an unchanged subagent result changed the parent workspace"
git merge-base --is-ancestor "$agent_result" "$head6" \
  || fail "merged subagent result is absent from parent workspace ancestry"

echo "== durable Escape at a model boundary ==" >&2
tree6=$(git rev-parse "$head6^{tree}")
user7=$(mkcommit "$tree6" \
  '{"author":"user","content":"this prompt was accidental"}' \
  "$head6")
request7=$("$CAOS_CLI" prepare-request "$llm" -- --head:commit="$user7")
admitted7=$(mkcommit "$tree6" \
  "{\"request\":\"$request7\",\"request_head\":\"$user7\",\"status\":\"queued\"}" \
  "$user7")
git push --quiet caos "$admitted7:$conversation_ref" \
  || fail "publishing Escape-test queued event"
"$CAOS_CLI" run "$request7" -- >/tmp/llm-step-result-7 2>/tmp/llm-step-error-7 &
interrupt_pid=$!

request_seen=0
for _ in $(seq 1 300); do
  if [ -e stub/request-15.json ]; then
    request_seen=1
    break
  fi
  if ! kill -0 "$interrupt_pid" 2>/dev/null; then
    fail "Escape-test turn exited before its blocked response: $(cat /tmp/llm-step-error-7)"
  fi
  sleep 0.2
done
[ "$request_seen" -eq 1 ] || fail "Escape-test model request never arrived"

escape_parent=$(fetch_head)
escape_tree=$(git rev-parse "$escape_parent^{tree}")
escape_commit=$(mkcommit "$escape_tree" \
  "{\"escape\":{\"request\":\"$request7\"}}" \
  "$escape_parent")
git push --quiet --force-with-lease="$conversation_ref:$escape_parent" \
  caos "$escape_commit:$conversation_ref" || fail "publishing Escape event"
printf '%s\n' \
  '{"content":[{"text":"I had started this response.","type":"text"},{"id":"toolu_interrupted","input":{"file_path":"interrupted.txt","content":"must not run\n"},"name":"write","type":"tool_use"}],"stop_reason":"tool_use"}' \
  > stub/response-15.json

for _ in $(seq 1 300); do
  if [ -e stub/request-16.json ]; then
    fail "Escape allowed another model round"
  fi
  if ! kill -0 "$interrupt_pid" 2>/dev/null; then
    break
  fi
  sleep 0.2
done
if ! wait "$interrupt_pid"; then
  fail "running Escape-test turn: $(cat /tmp/llm-step-error-7)"
fi
interrupt_pid=""

head7=$(fetch_head)
events7=$(git log --first-parent --format=%B "$user7..$head7")
terminal7=$(git show -s --format=%B "$head7")
grep -qF "\"request\":\"$request7\"" <<<"$events7" \
  || fail "Escape-test request identity was lost"
grep -qF '"escape"' <<<"$events7" || fail "Escape event is missing"
grep -qF 'I had started this response.' <<<"$events7" \
  || fail "in-flight model response was not recorded"
grep -qF '"tool_use_id":"toolu_interrupted"' <<<"$events7" \
  || fail "interrupted tool result is missing"
grep -qF 'interrupted before this tool ran' <<<"$events7" \
  || fail "interrupted tool was not closed explicitly"
grep -qF '"is_error":true' <<<"$events7" \
  || fail "interrupted tool result is not an error"
grep -qF '"status":"idle"' <<<"$terminal7" \
  || fail "Escape did not return the conversation to idle"
grep -qF '"interrupted":true' <<<"$terminal7" \
  || fail "Escape terminal status is not distinguishable"
if git cat-file -e "$head7:interrupted.txt" 2>/dev/null; then
  fail "Escape allowed the pending write tool to run"
fi
[ ! -e stub/request-16.json ] || fail "Escape allowed another model round"
[ -n "$(remote_exact_ref "refs/caos/res/$request7")" ] \
  || fail "interrupted exact request Q has no result ref"

echo "llm-step: ALL PASS" >&2
