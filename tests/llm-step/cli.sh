#!/usr/bin/env bash
# End-to-end v2 conversation test with a scripted LLM. The worker owns the
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
fetch_head() {
  local head
  head=$(remote_head)
  [ -n "$head" ] || fail "canonical conversation head is absent"
  git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$head" \
    || fail "fetching canonical conversation head"
  printf '%s\n' "$head"
}
assert_event_spine() { # <head> <stop>
  local current=$1 stop=$2 count=0 message
  while [ "$current" != "$stop" ]; do
    message=$(git show -s --format=%B "$current")
    grep -Eq '"v"[[:space:]]*:[[:space:]]*2' <<<"$message" \
      || fail "non-v2 commit on event spine: $current"
    current=$(git rev-parse "$current^")
    count=$((count + 1))
  done
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
cleanup() {
  if [ -n "$stub_pid" ]; then
    kill "$stub_pid" 2>/dev/null || true
  fi
  if [ -n "$gate_pid" ]; then
    kill "$gate_pid" 2>/dev/null || true
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

for _ in 1 2 3 4 5; do
  port=$((20000 + RANDOM % 20000))
  "$stub_bin" "0.0.0.0:$port" "$PWD/stub" 2>stub/log &
  stub_pid=$!
  sleep 0.5
  if kill -0 "$stub_pid" 2>/dev/null; then break; fi
  stub_pid=""
done
[ -n "$stub_pid" ] || fail "could not start llm-stub: $(cat stub/log)"

echo "== dispatch first turn ==" >&2
conv="conv-$(printf '%s' "${CAOS_SALT:-dev}" | tr -cd '0-9a-zA-Z')"
conversation_ref="refs/caos/conversations/$conv/head"
llm=$("$CAOS_CLI" curry DEEP-DEPS/llm-step -- \
  --api-key=test-key --system:@=system.txt \
  --model=test-model --base-url="http://$stub_host:$port" \
  --conversation="$conv")

user1=$(mkcommit "HEAD:ws" \
  '{"author":"user","content":"create out.txt containing hi, then confirm","status":"queued","v":2}' \
  "$base")
request1=$("$CAOS_CLI" prepare-request "$llm" -- --head:commit="$user1")
[ "${#request1}" -eq 40 ] && [[ "$request1" =~ ^[0-9a-f]+$ ]] \
  || fail "first prepared request is not exact Q: $request1"
early_interjection=$(mkcommit "HEAD:ws" \
  "{\"author\":\"user\",\"content\":\"$EARLY_INTERJECTION_TEXT\",\"username\":\"racer\",\"v\":2}" \
  "$user1")
git push --quiet caos "$early_interjection:$conversation_ref" \
  || fail "publishing queued event and pre-start interjection"
"$CAOS_CLI" run "$request1" -- >/tmp/llm-step-result || fail "running first turn"
[ -n "$(git ls-remote caos "refs/caos/res/$request1")" ] \
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
terminal1=$(git show -s --format=%B "$head1")
grep -Eq '"status"[[:space:]]*:[[:space:]]*"idle"' <<<"$terminal1" \
  || fail "turn did not become idle"
grep -Eq '"request"[[:space:]]*:[[:space:]]*null' <<<"$terminal1" \
  || fail "turn did not clear its request"
[ "$(git show "$head1:out.txt")" = hi ] || fail "out.txt missing from canonical head"
[ "$(git show "$head1:notes/todo.txt")" = "hello notes" ] || fail "untouched subtree lost"
[ "$(git ls-tree "$head1" run.sh | cut -d' ' -f1)" = 100755 ] \
  || fail "executable mode changed"

echo "== model replay and second turn ==" >&2
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
  '{"author":"user","content":"and now?","status":"queued","v":2}' "$head1")
request2=$("$CAOS_CLI" prepare-request "$llm" -- --head:commit="$user2")
[ "${#request2}" -eq 40 ] && [[ "$request2" =~ ^[0-9a-f]+$ ]] \
  || fail "second prepared request is not exact Q: $request2"
git push --quiet caos "$user2:$conversation_ref" || fail "publishing second queued event"
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
[ "$(git rev-parse "$running2^1")" = "$user2" ] \
  || fail "worker running event is not immediately after the queued event"
running2_event=$(git show -s --format=%B "$running2")
grep -qF "\"request\":\"$request2\"" <<<"$running2_event" \
  || fail "worker did not record the second running request"
grep -qF '"status":"running"' <<<"$running2_event" \
  || fail "worker request event is not running"
interjection=$(mkcommit "$tree1" \
  "{\"author\":\"user\",\"content\":\"$INTERJECTION_TEXT\",\"username\":\"racer\",\"v\":2}" \
  "$observed")
git push --quiet --force-with-lease="$conversation_ref:$observed" \
  caos "$interjection:$conversation_ref" || fail "publishing terminal-race interjection"

printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$STALE_T2_TEXT" > stub/response-4.json
if ! wait "$run2_pid"; then
  fail "running interjected second turn: $(cat /tmp/llm-step-error-2)"
fi
[ -n "$(git ls-remote caos "refs/caos/res/$request2")" ] \
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
grep -Eq '"request"[[:space:]]*:[[:space:]]*null' <<<"$terminal2" \
  || fail "second turn retained its request"
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
  '{"author":"user","content":"queue the independent request","status":"queued","v":2}' \
  "$head2")
request3=$("$CAOS_CLI" prepare-request "$llm" -- --head:commit="$user3")
[ "${#request3}" -eq 40 ] && [[ "$request3" =~ ^[0-9a-f]+$ ]] \
  || fail "third prepared request is not exact Q: $request3"
git push --quiet caos "$user3:$conversation_ref" \
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
mapfile -t pending_tasks < <(git ls-tree --name-only "$head3:.caos/async" | grep -E '^[0-9a-f]{40}$')
[ "${#pending_tasks[@]}" -eq 1 ] || fail "run_async did not record exactly one task"
task=${pending_tasks[0]}
[ "${#task}" -eq 40 ] && [[ "$task" =~ ^[0-9a-f]+$ ]] \
  || fail "pending tree has invalid task Q: $task"
[ "$(git show "$head3:.caos/async/$task/status")" = pending ] \
  || fail "primary idle tree does not retain pending status"
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
[ -z "$(git ls-remote caos "refs/caos/res/$task")" ] \
  || fail "independent task published a result before release"

echo "== independent completion and later observation ==" >&2
printf '{"content":[],"stop_reason":"end_turn"}' > async-gate/response-1.json
completion_head=""
for _ in $(seq 1 300); do
  candidate=$(remote_head)
  if [ "$candidate" != "$head3" ]; then
    git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$candidate" \
      || fail "fetching independent completion"
    if [ "$(git show "$candidate:.caos/async/$task/status" 2>/dev/null || true)" = complete ]; then
      completion_head=$candidate
      break
    fi
  fi
  sleep 0.2
done
[ -n "$completion_head" ] || fail "independent completion was not appended"
[ "$(git rev-parse "$completion_head^1")" = "$head3" ] \
  || fail "independent completion did not append after the primary turn"
changed=$(git diff --name-only "$head3" "$completion_head")
[ "$changed" = ".caos/async/$task/status" ] \
  || fail "independent completion changed unexpected paths: $changed"

task_result=""
for _ in $(seq 1 300); do
  task_result=$(git ls-remote caos "refs/caos/res/$task")
  if [ -n "$task_result" ]; then break; fi
  sleep 0.2
done
[ -n "$task_result" ] || fail "independent task Q has no result ref"

tree3=$(git rev-parse "$completion_head^{tree}")
user4=$(mkcommit "$tree3" \
  '{"author":"user","content":"what completed?","status":"queued","v":2}' \
  "$completion_head")
request4=$("$CAOS_CLI" prepare-request "$llm" -- --head:commit="$user4")
git push --quiet caos "$user4:$conversation_ref" \
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

echo "llm-step: ALL PASS" >&2
