#!/bin/bash
# shellcheck disable=SC1090,SC1091,SC2034,SC2154
set -euo pipefail

# DISABLED: flaky under load. The parent and child agents race for the stub's next
# reply, so this test answers each request by hand and polls for request-N.json for
# 60s (`wait_for_file`); on CI request 2 never arrived. To come back, script the
# replies by request instead of by arrival order. See tests/README.md, "Disabling a
# test"; delete this block to re-enable it.
echo "llm-subagent-path: SKIPPED (disabled)" > /tmp/report
caos put /tmp/report /cas/out
exit 0

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
source /cas/args/common

stage "source tree and scripted parent/child model"
llm_test_setup

rm -rf /tmp/ws
mkdir -p /tmp/ws
echo "hello" > /tmp/ws/original.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the source tree")

# The three operations are tools like any other: a directory whose `.caos-expr` names
# an image (the built entry, by hash). The parent stages them with `write`, then runs
# spawn by path in the same response (calls run in order, so the entries exist by then).
spawn_img=$(caos hash /cas/args/agent-spawn)
wait_img=$(caos hash /cas/args/agent-wait)
harvest_img=$(caos hash /cas/args/agent-harvest)
async_img=$(caos hash /cas/args/run-async)
# A request the independent task will run: std/hello, a worker that answers at once.
async_request=$(caos prepare-request --base:hash="$(caos hash /cas/args/hello)" --greeting="from-$SALT")
assert_oid "$async_request" "the independent request"
PROMPT="write child-output.txt with the delegated result"
rm -rf /tmp/stub
mkdir -p /tmp/stub
jq -n --arg s "$spawn_img" --arg w "$wait_img" --arg h "$harvest_img" --arg a "$async_img" --arg request "$async_request" --arg prompt "$PROMPT" '{content:[
  {type:"tool_use",id:"stage_spawn",name:"write",input:{"file-path":"tools/agent-spawn/.caos-expr",content:("curry --base:hash=" + $s + "\n")}},
  {type:"tool_use",id:"stage_wait",name:"write",input:{"file-path":"tools/agent-wait/.caos-expr",content:("curry --base:hash=" + $w + "\n")}},
  {type:"tool_use",id:"stage_harvest",name:"write",input:{"file-path":"tools/agent-harvest/.caos-expr",content:("curry --base:hash=" + $h + "\n")}},
  {type:"tool_use",id:"stage_async",name:"write",input:{"file-path":"tools/run-async/.caos-expr",content:("curry --base:hash=" + $a + "\n")}},
  {type:"tool_use",id:"toolu_help",name:"tool_help",input:{path:"tools/agent-spawn"}},
  {type:"tool_use",id:"toolu_async",name:"run_tool",input:{path:"tools/run-async",arguments:{request:$request}}},
  {type:"tool_use",id:"toolu_spawn",name:"run_tool",input:{path:"tools/agent-spawn",arguments:{prompt:$prompt}}}],
  stop_reason:"tool_use"}' > /tmp/stub/response-1.json
mkfifo /tmp/stub/response-2.json /tmp/stub/response-3.json /tmp/stub/response-4.json \
  /tmp/stub/response-5.json /tmp/stub/response-6.json
start_stub /tmp/stub

new_llm_conversation llm-subagent-path "$STUB_PORT" "$ws" \
  "You are a coding agent operating on a git source_tree."
LLM_TEST_USERNAME=Alice
admit_turn "delegate a focused edit"
request1=$request
start_turn

# The parent and the child race for the next model slot; the stub is sequential, so each
# blocked request is read and its FIFO released with the answer belonging to that
# conversation: the child writes and then reports, the parent waits, harvests, reports.
child_writes=0
child_reports=0
parent_waits=0
parent_harvests=0
parent_reports=0
for request_number in 2 3 4 5 6; do
  if ! wait_for_file "/tmp/stub/request-$request_number.json"; then
    dump_conversation "$conversation_ref" "$request1"
    fail "parent/child API request $request_number never arrived"
  fi
  response="/tmp/stub/response-$request_number.json"
  if jq -e '.system | contains("You are a focused subagent")' "/tmp/stub/request-$request_number.json" >/dev/null; then
    if [ "$child_writes" -eq 0 ]; then
      printf '%s\n' '{"content":[{"id":"toolu_child_write","input":{"content":"written by child\n","file-path":"main/child-output.txt"},"name":"write","type":"tool_use"}],"stop_reason":"tool_use"}' > "$response"
      child_writes=1
    else
      printf '%s\n' '{"content":[{"text":"child done","type":"text"}],"stop_reason":"end_turn"}' > "$response"
      child_reports=1
    fi
  elif grep -qF '"tool_use_id":"toolu_harvest"' "/tmp/stub/request-$request_number.json"; then
    printf '%s\n' '{"content":[{"text":"parent done","type":"text"}],"stop_reason":"end_turn"}' > "$response"
    parent_reports=1
  elif grep -qF '"tool_use_id":"toolu_wait"' "/tmp/stub/request-$request_number.json"; then
    printf '{"content":[{"id":"toolu_harvest","input":{"path":"tools/agent-harvest","arguments":{"child":"%s"}},"name":"run_tool","type":"tool_use"}],"stop_reason":"tool_use"}\n' "$child_id" > "$response"
    parent_harvests=1
  else
    wait_head=$(remote_tip "$conversation_ref") || fail "the parent could not read the conversation head"
    child_id=$($TOOL children --repo /tmp/repo --head "$wait_head" | jq -r '.id')
    child_id=${child_id%%$'\n'*}
    [ -n "$child_id" ] || fail "the parent could not find the child"
    printf '{"content":[{"id":"toolu_wait","input":{"path":"tools/agent-wait","arguments":{"child":"%s"}},"name":"run_tool","type":"tool_use"}],"stop_reason":"tool_use"}\n' "$child_id" > "$response"
    parent_waits=1
  fi
done
[ "$child_writes$child_reports$parent_waits$parent_harvests$parent_reports" = 11111 ] \
  || fail "the race did not play out: $child_writes$child_reports$parent_waits$parent_harvests$parent_reports"

wait_turn || fail "the parent turn never reached a terminal event"
head1=$head
$TOOL tools --repo /tmp/repo --head "$head1" --request "$request1" > /tmp/parent-tools.jsonl

stage "each operation is recorded as the run_tool call the model made, and completed"
for id in toolu_spawn toolu_wait toolu_harvest toolu_async; do
  record=$(jq -c --arg id "$id" 'select(.id == $id)' /tmp/parent-tools.jsonl)
  [ -n "$record" ] || fail "$id has no tool record"
  [ "$(jq -r '.name' <<<"$record")" = run_tool ] || fail "$id is recorded as $(jq -r '.name' <<<"$record"), not run_tool"
  [ "$(jq -r '.status' <<<"$record")" = complete ] || fail "$id is $(jq -r '.status' <<<"$record"), not complete"
done

stage "spawn made the child, wait joined its relay, harvest applied its file"
$TOOL tool-observation --repo /tmp/repo --head "$head1" --request "$request1" --round 0 --id toolu_spawn > /tmp/spawn-observation.json
child=$(jq -r '.child // empty' /tmp/spawn-observation.json)
[ "${#child}" -gt 10 ] || fail "spawn returned no child id: $(cat /tmp/spawn-observation.json)"
$TOOL children --repo /tmp/repo --head "$head1" > /tmp/children.jsonl
relay=$(jq -r --arg child "$child" 'select(.id == $child) | .relay' /tmp/children.jsonl)
assert_oid "$relay" "child relay"
wait_record=$(jq -c 'select(.id == "toolu_wait")' /tmp/parent-tools.jsonl)
[ "$(jq -r '.task' <<<"$wait_record")" = "$relay" ] || fail "wait did not join the child's relay"
$TOOL tool-observation --repo /tmp/repo --head "$head1" --request "$request1" --round 1 --id toolu_wait > /tmp/wait-observation.json
[ "$(jq -r '.child' /tmp/wait-observation.json)" = "$child" ] || fail "wait names the wrong child"
[ "$(jq -r '.status' /tmp/wait-observation.json)" = completed ] || fail "wait did not report the child completed"
source_tree=$(source_tree_commit "$head1" main)
fetch_code "$source_tree" "fetching the parent's source tree"
[ "$(git show "$source_tree:child-output.txt")" = "written by child" ] \
  || fail "harvest did not bring the child's file into the parent"
[ "$(git show "$source_tree:original.txt")" = "hello" ] || fail "harvest lost the parent's own file"
stage "run-async by path started the independent task"
$TOOL async --repo /tmp/repo --head "$head1" > /tmp/async.tasks
[ "$(jq -s 'length' /tmp/async.tasks)" -eq 1 ] || fail "run-async did not record exactly one task: $(cat /tmp/async.tasks)"
async_task=$(jq -r '.task' /tmp/async.tasks)
assert_oid "$async_task" "the async task"
$TOOL tool-observation --repo /tmp/repo --head "$head1" --request "$request1" --round 0 --id toolu_async > /tmp/async-observation.json
grep -qF "$async_task" /tmp/async-observation.json || fail "run-async's answer omitted the task: $(cat /tmp/async-observation.json)"
assert_spine "$head1"
pass llm-subagent-path
