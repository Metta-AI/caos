#!/usr/bin/env bash
# Core llm-step conversation behavior: durable event recording, ordered tool
# replay, workspace mutation, and interjections on both sides of dispatch. The
# independent-work, subagent, and Escape paths live in sibling tests so these
# otherwise serial scenarios can fan out.
set -euo pipefail
# The dependency is mounted only inside the test wrapper and exports globals.
# shellcheck disable=SC1091
source DEEP-DEPS/llm-test/common.sh

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

stage "workspace and scripted model"
llm_test_setup
stub_host="${stub_host:?llm_test_setup did not set stub_host}"
mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
echo "You are a coding agent operating on a git workspace." > system.txt
git add -A
gc commit -qm fixtures
base=$(mkcommit "HEAD:ws" base)

R1='[{"signature":"sig-abc","thinking":"I should create the file.","type":"thinking"},{"text":"Creating out.txt.","type":"text"},{"id":"toolu_01","input":{"cmd":"echo hi > out.txt","paths":[]},"name":"bash","type":"tool_use"}]'
R2='[{"id":"toolu_03","input":{"cmd":"echo boom >&2; exit 3","paths":[]},"name":"bash","type":"tool_use"}]'
EARLY_INTERJECTION_TEXT="also keep the notes subtree"
INTERJECTION_TEXT="one more thing before you finish"
STALE_T2_TEXT="the workspace still holds out.txt"
T2_TEXT="yes, I also saw your last message"

mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1" > stub/response-1.json
printf '{"content":%s,"stop_reason":"tool_use"}' "$R2" > stub/response-2.json
printf '%s\n' \
  '{"content":[{"text":"done: out.txt contains hi","type":"text"}],"stop_reason":"end_turn"}' \
  > stub/response-3.json
# Hold the second turn at its terminal model response so an interjection can
# race with it deterministically.
mkfifo stub/response-4.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$T2_TEXT" > stub/response-5.json

stub_pid=""
port=""
start_stub stub stub_pid port
stub_pid="${stub_pid:?start_stub did not set stub_pid}"
new_llm_conversation llm-step "$port"
conv="${conv:?new_llm_conversation did not set conv}"
conversation_ref="${conversation_ref:?new_llm_conversation did not set conversation_ref}"
llm="${llm:?new_llm_conversation did not set llm}"

stage "first turn: durable tool events and workspace"
user1=$(mkcommit "HEAD:ws" \
  "{\"base\":\"$base\",\"author\":\"user\",\"content\":\"create out.txt containing hi, then confirm\"}" \
  "$base")
request1=$("$CAOS_CLI" prepare-request --base:hash="$llm" --head:commit="$user1")
assert_oid "$request1" "first prepared request"
admitted1=$(mkcommit "HEAD:ws" \
  "{\"request\":\"$request1\",\"request_head\":\"$user1\",\"status\":\"queued\"}" \
  "$user1")
early_interjection=$(mkcommit "HEAD:ws" \
  "{\"author\":\"user\",\"content\":\"$EARLY_INTERJECTION_TEXT\",\"username\":\"racer\"}" \
  "$admitted1")
git push --quiet caos "$early_interjection:$conversation_ref" \
  || fail "publishing queued event and pre-start interjection"
"$CAOS_CLI" run --base:hash="$request1" >/tmp/llm-step-result \
  || fail "running first turn"

head1=$(fetch_head)
assert_event_spine "$head1" "$base"
events1=$(git log --first-parent --format=%B "$base..$head1")
grep -qF "\"request\":\"$request1\"" <<<"$events1" \
  || fail "worker did not record the running request"
grep -qF "$EARLY_INTERJECTION_TEXT" <<<"$events1" \
  || fail "pre-start interjection was lost"
grep -Eq '"response"[[:space:]]*:[[:space:]]*\[' <<<"$events1" \
  || fail "model response record is missing"
grep -Eq '"calls"[[:space:]]*:[[:space:]]*\[' <<<"$events1" \
  || fail "ordered call record is missing"
grep -qF '"tool_use_id":"toolu_01"' <<<"$events1" \
  || fail "first tool result was not recorded"
grep -qF '"tool_use_id":"toolu_03"' <<<"$events1" \
  || fail "second-round result was not recorded"
grep -qF '"is_error":true' <<<"$events1" \
  || fail "failed tool result lost its error bit"
grep -qF 'done: out.txt contains hi' <<<"$events1" \
  || fail "assistant transcript was not recorded"
terminal1=$(git show -s --format=%B "$head1")
grep -qF '"status":"idle"' <<<"$terminal1" \
  || fail "turn did not become idle"
[ "$(git show "$head1:out.txt")" = hi ] || fail "out.txt missing from canonical head"
[ "$(git show "$head1:notes/todo.txt")" = "hello notes" ] \
  || fail "untouched subtree lost"

stage "exact replay and terminal-race interjection"
grep -qF "\"messages\":[{\"content\":\"create out.txt containing hi, then confirm\",\"role\":\"user\"},{\"content\":\"$EARLY_INTERJECTION_TEXT\",\"role\":\"user\"}]" \
  stub/request-1.json || fail "queued messages were not replayed"
grep -qF "\"content\":$R1,\"role\":\"assistant\"" stub/request-2.json \
  || fail "round-one response was not replayed verbatim"
grep -qF '"tool_use_id":"toolu_01","type":"tool_result"' stub/request-2.json \
  || fail "round-one result missing"
grep -qF "\"content\":$R2,\"role\":\"assistant\"" stub/request-3.json \
  || fail "round-two response was not replayed verbatim"
grep -qF 'exit: 3' stub/request-3.json || fail "failed command result missing"
[ ! -f stub/request-4.json ] || fail "unexpected extra model round"

tree1=$(git rev-parse "$head1^{tree}")
user2=$(mkcommit "$tree1" '{"author":"user","content":"and now?"}' "$head1")
request2=$("$CAOS_CLI" prepare-request --base:hash="$llm" --head:commit="$user2")
assert_oid "$request2" "second prepared request"
admitted2=$(mkcommit "$tree1" \
  "{\"request\":\"$request2\",\"request_head\":\"$user2\",\"status\":\"queued\"}" \
  "$user2")
git push --quiet caos "$admitted2:$conversation_ref" \
  || fail "publishing second request admission"
"$CAOS_CLI" run --base:hash="$request2" >/tmp/llm-step-result-2 2>/tmp/llm-step-error-2 &
run2_pid=$!
LLM_TEST_PIDS+=("$run2_pid")

request_started=0
for _ in $(seq 1 150); do
  if [ -e stub/request-4.json ]; then request_started=1; break; fi
  if ! kill -0 "$run2_pid" 2>/dev/null; then
    fail "second turn exited before its blocked response: $(cat /tmp/llm-step-error-2)"
  fi
  sleep 0.2
done
[ "$request_started" -eq 1 ] || fail "second turn never reached the blocked response"

running2=$(fetch_head)
[ "$(git rev-parse "$running2^1")" = "$admitted2" ] \
  || fail "running event is not immediately after admission"
grep -qF '"status":"running"' <<<"$(git show -s --format=%B "$running2")" \
  || fail "worker request event is not running"
interjection=$(mkcommit "$tree1" \
  "{\"author\":\"user\",\"content\":\"$INTERJECTION_TEXT\",\"username\":\"racer\"}" \
  "$running2")
git push --quiet --force-with-lease="$conversation_ref:$running2" \
  caos "$interjection:$conversation_ref" || fail "publishing terminal-race interjection"
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$STALE_T2_TEXT" > stub/response-4.json
if ! wait "$run2_pid"; then
  fail "running interjected second turn: $(cat /tmp/llm-step-error-2)"
fi

head2=$(fetch_head)
assert_event_spine "$head2" "$base"
[ "$(git rev-parse "$head2^{tree}")" = "$tree1" ] \
  || fail "toolless turn changed the workspace"
[ "$(git rev-parse "$head2^1")" = "$interjection" ] \
  || fail "fresh response is not immediately after the interjection"
terminal2=$(git show -s --format=%B "$head2")
grep -qF "$T2_TEXT" <<<"$terminal2" || fail "interjection answer is not terminal"
events2=$(git log --first-parent --format=%B "$running2..$head2")
grep -qF "$INTERJECTION_TEXT" <<<"$events2" || fail "racing interjection was lost"
if grep -qF "$STALE_T2_TEXT" <<<"$events2"; then
  fail "stale pre-interjection response became canonical"
fi
if grep -qF "$INTERJECTION_TEXT" stub/request-4.json; then
  fail "racing interjection leaked into the in-flight request"
fi
if grep -qF "$STALE_T2_TEXT" stub/request-5.json; then
  fail "discarded response was replayed to the model"
fi
grep -qF "{\"content\":\"$INTERJECTION_TEXT\",\"role\":\"user\"}]" stub/request-5.json \
  || fail "racing interjection was not replayed in the replacement call"

stage "done"
echo "llm-step: ALL PASS" >&2
