#!/bin/bash
# tests/llm-interrupt — a WORKER test, in dev/worker-test (it needs git).
#
# Escape is a durable event, not a client-side cancellation: an in-flight model
# response is recorded, its pending tools are closed as errors without running,
# and the exact request still receives an idle interrupted result.
#
# NO CLIENT AT ALL, which suits this test better than it suited the old one.
# The original ran the turn in a background client and interrupted it; the claim
# is precisely that interruption does NOT depend on a caller going away, so
# publishing the Escape event to the ref — with nothing holding the turn open —
# is the stronger demonstration.
#
# THE MODEL ROUND IS HELD OPEN BY A FIFO. The stub blocks reading response-1
# until this script releases it, so "the Escape arrived mid-round" is a fact the
# test controls rather than races against.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "workspace and blocked model response"
llm_test_setup

rm -rf /tmp/ws && mkdir -p /tmp/ws
echo "hello" > /tmp/ws/greeting.txt
caos put /tmp/ws /cas/ws >/dev/null || fail "publishing the workspace"
ws=$(caos hash /cas/ws)

rm -rf /tmp/stub && mkdir -p /tmp/stub
mkfifo /tmp/stub/response-1.json
stub_pid=""
port=""
start_stub /tmp/stub stub_pid port

new_llm_conversation llm-interrupt "$port" "$ws" \
  "You are a coding agent operating on a git workspace."

stage "publish Escape at the model boundary"
dispatch_turn "$ws" "this prompt was accidental"
user1=$human
request1=$request

request_seen=0
for _ in $(seq 1 300); do
  if [ -e /tmp/stub/request-1.json ]; then request_seen=1; break; fi
  sleep 0.2
done
[ "$request_seen" -eq 1 ] || fail "model request never arrived"

# ON TOP OF WHATEVER THE TURN HAS ALREADY APPENDED (llm-step publishes a
# `running` event before calling the model), with --force-with-lease so a race
# is a failed push rather than a lost event.
escape_parent=$(current_head)
escape_tree=$(git rev-parse "$escape_parent^{tree}")
escape_commit=$(mint_commit /cas/escape "$escape_tree" \
  "{\"escape\":{\"request\":\"$request1\"}}" "$escape_parent")
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$escape_commit" \
  || fail "fetching the escape commit back"
git push --quiet --force-with-lease="$conversation_ref:$escape_parent" \
  caos "$escape_commit:$conversation_ref" || fail "publishing Escape event"

# Now let the held round return — with a tool call in it, which must NOT run.
printf '%s\n' \
  '{"content":[{"text":"I had started this response.","type":"text"},{"id":"toolu_interrupted","input":{"file_path":"interrupted.txt","content":"must not run\n"},"name":"write","type":"tool_use"}],"stop_reason":"tool_use"}' \
  > /tmp/stub/response-1.json

head1=$(wait_turn) || {
  echo "--- stub log" >&2; cat /tmp/stub/log >&2 || true
  fail "the interrupted turn never reached a terminal event"
}

stage "interrupted response is durable and tools did not run"
events1=$(git log --first-parent --format=%B "$user1..$head1")
terminal1=$(git show -s --format=%B "$head1")
grep -qF "\"request\":\"$request1\"" <<<"$events1" \
  || fail "interrupted request identity was lost"
grep -qF 'I had started this response.' <<<"$events1" \
  || fail "in-flight model response was not recorded"
grep -qF '"tool_use_id":"toolu_interrupted"' <<<"$events1" \
  || fail "interrupted tool result is missing"
grep -qF 'interrupted before this tool ran' <<<"$events1" \
  || fail "interrupted tool was not closed explicitly"
grep -qF '"is_error":true' <<<"$events1" \
  || fail "interrupted tool result is not an error"
grep -qF '"status":"idle"' <<<"$terminal1" \
  || fail "Escape did not return the conversation to idle"
grep -qF '"interrupted":true' <<<"$terminal1" \
  || fail "Escape terminal status is not distinguishable"
if git cat-file -e "$head1:interrupted.txt" 2>/dev/null; then
  fail "Escape allowed the pending write tool to run"
fi
[ ! -e /tmp/stub/request-2.json ] || fail "Escape allowed another model round"

stage "done"
printf 'llm-interrupt: ALL PASS\n' > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
