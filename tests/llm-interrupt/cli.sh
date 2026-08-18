#!/usr/bin/env bash
# Escape is a durable event, not a client-side cancellation: an in-flight model
# response is recorded, its pending tools are closed as errors without running,
# and the exact request still receives an idle interrupted result.
set -euo pipefail
# The dependency is mounted only inside the test wrapper and exports globals.
# shellcheck disable=SC1091
source DEEP-DEPS/llm-test/common.sh

stage "workspace and blocked model response"
llm_test_setup
stub_host="${stub_host:?llm_test_setup did not set stub_host}"
mkdir ws
echo "hello" > ws/greeting.txt
echo "You are a coding agent operating on a git workspace." > system.txt
git add -A
gc commit -qm fixtures
base=$(mkcommit "HEAD:ws" base)

mkdir stub
mkfifo stub/response-1.json
stub_pid=""
port=""
start_stub stub stub_pid port
stub_pid="${stub_pid:?start_stub did not set stub_pid}"
new_llm_conversation llm-interrupt "$port"
conv="${conv:?new_llm_conversation did not set conv}"
conversation_ref="${conversation_ref:?new_llm_conversation did not set conversation_ref}"
llm="${llm:?new_llm_conversation did not set llm}"

stage "publish Escape at the model boundary"
user1=$(mkcommit "HEAD:ws" \
  "{\"base\":\"$base\",\"author\":\"user\",\"content\":\"this prompt was accidental\"}" \
  "$base")
request1=$("$CAOS_CLI" prepare-request --base:hash="$llm" --head:commit="$user1")
assert_oid "$request1" "interrupted prepared request"
admitted1=$(mkcommit "HEAD:ws" \
  "{\"request\":\"$request1\",\"request_head\":\"$user1\",\"status\":\"queued\"}" \
  "$user1")
git push --quiet caos "$admitted1:$conversation_ref" \
  || fail "publishing interrupted request admission"
"$CAOS_CLI" run --base:hash="$request1" >/tmp/llm-interrupt-result 2>/tmp/llm-interrupt-error &
interrupt_pid=$!
LLM_TEST_PIDS+=("$interrupt_pid")

request_seen=0
for _ in $(seq 1 300); do
  if [ -e stub/request-1.json ]; then request_seen=1; break; fi
  if ! kill -0 "$interrupt_pid" 2>/dev/null; then
    fail "turn exited before its blocked response: $(cat /tmp/llm-interrupt-error)"
  fi
  sleep 0.2
done
[ "$request_seen" -eq 1 ] || fail "model request never arrived"

escape_parent=$(fetch_head)
escape_tree=$(git rev-parse "$escape_parent^{tree}")
escape_commit=$(mkcommit "$escape_tree" \
  "{\"escape\":{\"request\":\"$request1\"}}" "$escape_parent")
git push --quiet --force-with-lease="$conversation_ref:$escape_parent" \
  caos "$escape_commit:$conversation_ref" || fail "publishing Escape event"
printf '%s\n' \
  '{"content":[{"text":"I had started this response.","type":"text"},{"id":"toolu_interrupted","input":{"file_path":"interrupted.txt","content":"must not run\n"},"name":"write","type":"tool_use"}],"stop_reason":"tool_use"}' \
  > stub/response-1.json

for _ in $(seq 1 300); do
  if [ -e stub/request-2.json ]; then fail "Escape allowed another model round"; fi
  if ! kill -0 "$interrupt_pid" 2>/dev/null; then break; fi
  sleep 0.2
done
if ! wait "$interrupt_pid"; then
  fail "running interrupted turn: $(cat /tmp/llm-interrupt-error)"
fi

stage "interrupted response is durable and tools did not run"
head1=$(fetch_head)
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
[ ! -e stub/request-2.json ] || fail "Escape allowed another model round"
[ -n "$(remote_exact_ref "refs/caos/res/$request1")" ] \
  || fail "interrupted exact request has no result ref"

stage "done"
echo "llm-interrupt: ALL PASS" >&2
