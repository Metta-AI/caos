#!/usr/bin/env bash
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI)
# ----------------------------------------------------------
# Interrupt handling is not a property of the server, the model, or any stub —
# it is a property of the tested client's turn loop, and there is no other place
# it can be observed. `$CAOS_CLI run` is what fetches the conversation, drives
# the model round, and — crucially — reacts to an Escape event that is published
# against the conversation ref WHILE a response is in flight. So the CLI is the
# subject, not a convenience for reaching one: a substitute driver would be
# testing itself, not the code under test.
#
# Concretely, this test pins down that Escape is a DURABLE EVENT and not a
# client-side cancellation, and every clause of that claim is a decision made
# only inside the tested `run`:
#   - prepare-request/run must admit and begin the exact request ($request1),
#     so the request identity is preserved across the interrupt;
#   - when Escape is published mid-turn, the client must RECORD the partial,
#     in-flight model response as a real event rather than discarding it;
#   - it must CLOSE the response's pending tool_use as an is_error result
#     ("interrupted before this tool ran") WITHOUT executing the tool — proven
#     here by the `write` tool never creating interrupted.txt;
#   - it must NOT start another model round (no request-2.json); and
#   - it must land the conversation in a terminal idle+interrupted status.
# None of these are visible except by running a real turn through $CAOS_CLI
# against the inner stack and inspecting the events it commits. That is why the
# CLI is essential here, and why this test cannot be reduced to checking the
# subject directly.
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

stage "done"
echo "llm-interrupt: ALL PASS" >&2
