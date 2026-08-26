#!/bin/bash
# tests/llm-step — a WORKER test, in dev/worker-test (it needs git).
#
# Core llm-step conversation behavior: durable event recording, ordered tool
# replay, workspace mutation, and interjections on both sides of dispatch. The
# independent-work, subagent, and Escape paths live in sibling tests so these
# otherwise serial scenarios can fan out.
#
# All of it is llm-step's and all of it lands on the conversation ref, so there
# is nothing here a client was needed for beyond currying the turns and blocking
# on them — which worker-common.sh does without blocking.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

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

assert_caos_dates() { # <revision range>
  local range=$1 commit author timestamp
  for commit in $(git rev-list "$range"); do
    author=$(git show -s --format=%an "$commit")
    if [ "$author" = caos-agent ]; then
      timestamp=$(git show -s --format=%ct "$commit")
      [ "$timestamp" -gt 0 ] \
        || fail "caos-agent commit $commit still has an epoch-zero date"
    fi
  done
}

stage "workspace and scripted model"
llm_test_setup

rm -rf /tmp/ws && mkdir -p /tmp/ws/notes
echo "hello notes" > /tmp/ws/notes/todo.txt
caos put /tmp/ws /cas/ws >/dev/null || fail "publishing the workspace"
ws=$(caos hash /cas/ws)

R1='[{"signature":"sig-abc","thinking":"I should create the file.","type":"thinking"},{"text":"Creating out.txt.","type":"text"},{"id":"toolu_01","input":{"cmd":"echo hi > out.txt","paths":[]},"name":"bash","type":"tool_use"}]'
R2='[{"id":"toolu_03","input":{"cmd":"echo boom >&2; exit 3","paths":[]},"name":"bash","type":"tool_use"}]'
EARLY_INTERJECTION_TEXT="also keep the notes subtree"
INTERJECTION_TEXT="one more thing before you finish"
STALE_T2_TEXT="the workspace still holds out.txt"
T2_TEXT="yes, I also saw your last message"

rm -rf /tmp/stub && mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1" > /tmp/stub/response-1.json
printf '{"content":%s,"stop_reason":"tool_use"}' "$R2" > /tmp/stub/response-2.json
printf '%s\n' \
  '{"content":[{"text":"done: out.txt contains hi","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-3.json
# Hold the second turn at its terminal model response so an interjection can
# race with it deterministically.
mkfifo /tmp/stub/response-4.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$T2_TEXT" > /tmp/stub/response-5.json

stub_pid=""
port=""
start_stub /tmp/stub stub_pid port

new_llm_conversation llm-step "$port" "$ws" \
  "You are a coding agent operating on a git workspace."

stage "first turn: durable tool events and workspace"
# ADMITTED, THEN INTERJECTED, THEN STARTED: the interjection has to be on the
# spine before the worker begins, which is the pre-dispatch case.
admit_turn "$ws" "create out.txt containing hi, then confirm"
user1=$human
request1=$request
early_interjection=$(mint_commit /cas/early "$ws" \
  "{\"author\":\"user\",\"content\":\"$EARLY_INTERJECTION_TEXT\",\"username\":\"racer\"}" \
  "$admitted")
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$early_interjection" \
  || fail "fetching the pre-start interjection back"
git push --quiet caos "$early_interjection:$conversation_ref" \
  || fail "publishing the pre-start interjection"
start_turn

head1=$(wait_turn) || {
  echo "--- stub log" >&2; cat /tmp/stub/log >&2 || true
  fail "the first turn never reached a terminal event"
}
assert_event_spine "$head1" "$base"
assert_caos_dates "$base..$head1"
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
grep -qF '"status":"idle"' <<<"$terminal1" || fail "turn did not become idle"
[ "$(git show "$head1:out.txt")" = hi ] || fail "out.txt missing from canonical head"
[ "$(git show "$head1:notes/todo.txt")" = "hello notes" ] \
  || fail "untouched subtree lost"

stage "exact replay and terminal-race interjection"
grep -qF "\"messages\":[{\"content\":\"create out.txt containing hi, then confirm\",\"role\":\"user\"},{\"content\":\"$EARLY_INTERJECTION_TEXT\",\"role\":\"user\"}]" \
  /tmp/stub/request-1.json || fail "queued messages were not replayed"
grep -qF "\"content\":$R1,\"role\":\"assistant\"" /tmp/stub/request-2.json \
  || fail "round-one response was not replayed verbatim"
grep -qF '"tool_use_id":"toolu_01","type":"tool_result"' /tmp/stub/request-2.json \
  || fail "round-one result missing"
grep -qF "\"content\":$R2,\"role\":\"assistant\"" /tmp/stub/request-3.json \
  || fail "round-two response was not replayed verbatim"
grep -qF 'exit: 3' /tmp/stub/request-3.json || fail "failed command result missing"
[ ! -f /tmp/stub/request-4.json ] || fail "unexpected extra model round"

tree1=$(git rev-parse "$head1^{tree}")
dispatch_turn "$tree1" "and now?" "$head1"
admitted2=$admitted

request_started=0
for _ in $(seq 1 150); do
  if [ -e /tmp/stub/request-4.json ]; then request_started=1; break; fi
  sleep 0.2
done
[ "$request_started" -eq 1 ] || fail "second turn never reached the blocked response"

running2=$(current_head)
[ "$(git rev-parse "$running2^1")" = "$admitted2" ] \
  || fail "running event is not immediately after admission"
grep -qF '"status":"running"' <<<"$(git show -s --format=%B "$running2")" \
  || fail "worker request event is not running"
interjection=$(mint_commit /cas/interjection "$tree1" \
  "{\"author\":\"user\",\"content\":\"$INTERJECTION_TEXT\",\"username\":\"racer\"}" \
  "$running2")
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$interjection" \
  || fail "fetching the terminal-race interjection back"
git push --quiet --force-with-lease="$conversation_ref:$running2" \
  caos "$interjection:$conversation_ref" || fail "publishing terminal-race interjection"
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$STALE_T2_TEXT" > /tmp/stub/response-4.json

head2=$(wait_turn) || fail "the second turn never reached a terminal event"
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
if grep -qF "$INTERJECTION_TEXT" /tmp/stub/request-4.json; then
  fail "racing interjection leaked into the in-flight request"
fi
if grep -qF "$STALE_T2_TEXT" /tmp/stub/request-5.json; then
  fail "discarded response was replayed to the model"
fi
grep -qF "{\"content\":\"$INTERJECTION_TEXT\",\"role\":\"user\"}]" /tmp/stub/request-5.json \
  || fail "racing interjection was not replayed in the replacement call"

stage "done"
printf 'llm-step: ALL PASS\n' > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
