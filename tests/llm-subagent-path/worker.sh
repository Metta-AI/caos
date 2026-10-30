#!/bin/bash
# shellcheck disable=SC1090,SC1091,SC2034,SC2154
set -euo pipefail

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
start_stub /tmp/stub

new_llm_conversation llm-subagent-path "$STUB_PORT" "$ws" \
  "You are a coding agent operating on a git source_tree."
LLM_TEST_USERNAME=Alice
admit_turn "delegate a focused edit"

# The parent and the child share one stub and race for its next reply, so no reply can
# be tied to a request NUMBER. Each is tied to what the request SAYS instead (std/llm-stub,
# "rules"), and all of them are written before the turn starts. The test then only waits
# for the turn: nothing polls for a request, and the order the two agents arrive in does
# not matter.
#
# The parent's wait and harvest calls name the child. Its id is derived from the durable
# spawn call (the parent conversation and the call id), so it is
# known here, before the child exists.
child_id=$($TOOL child-id --parent "$conv" \
  --tool toolu_spawn) || fail "deriving the child id"
child_id=${child_id#child }
[ -n "$child_id" ] || fail "the child id is empty"

# rule NAME RESPONSE [NEEDLE | !NEEDLE]...: answer a request whose body has every NEEDLE
# and none of the !NEEDLEs. Rules are tried in name order, so the most specific is first.
rule() {
  local name=$1 response=$2 needle
  shift 2
  mkdir -p "/tmp/stub/rules/$name"
  printf '%s\n' "$response" > "/tmp/stub/rules/$name/response.json"
  : > "/tmp/stub/rules/$name/contains"
  : > "/tmp/stub/rules/$name/lacks"
  for needle in "$@"; do
    case "$needle" in
      '!'*) printf '%s\n' "${needle#!}" >> "/tmp/stub/rules/$name/lacks" ;;
      *) printf '%s\n' "$needle" >> "/tmp/stub/rules/$name/contains" ;;
    esac
  done
}
run_tool_call() {
  printf '{"content":[{"id":"%s","input":{"path":"%s","arguments":{"child":"%s"}},"name":"run_tool","type":"tool_use"}],"stop_reason":"tool_use"}' \
    "$1" "$2" "$child_id"
}
CHILD_SYSTEM='You are a focused subagent'

# The child (the only conversation with that system prompt): after its write, report;
# before it, write.
rule 01-child-reports \
  '{"content":[{"text":"child done","type":"text"}],"stop_reason":"end_turn"}' \
  "$CHILD_SYSTEM" '"tool_use_id":"toolu_child_write"'
rule 02-child-writes \
  '{"content":[{"id":"toolu_child_write","input":{"content":"written by child\n","file-path":"main/child-output.txt"},"name":"write","type":"tool_use"}],"stop_reason":"tool_use"}' \
  "$CHILD_SYSTEM"
# The parent, furthest along first: after harvest, report; after wait, harvest; and
# otherwise (after the first batch) wait.
rule 03-parent-reports \
  '{"content":[{"text":"parent done","type":"text"}],"stop_reason":"end_turn"}' \
  '"tool_use_id":"toolu_harvest"'
rule 04-parent-harvests "$(run_tool_call toolu_harvest tools/agent-harvest)" \
  '"tool_use_id":"toolu_wait"'
rule 05-parent-waits "$(run_tool_call toolu_wait tools/agent-wait)"

# On any failure below, say what the stub saw and what the conversation looks like.
diagnose_failure() {
  local status=$?
  if [ "$status" -ne 0 ]; then
    echo "--- llm-stub log ---" >&2
    cat /tmp/stub/log >&2 || true
    dump_conversation "$conversation_ref" "$request1" || true
  fi
  llm_test_cleanup
}
trap diagnose_failure EXIT

start_turn
wait_turn || fail "the parent turn never reached a terminal event"

stage "each scripted reply was used exactly once"
for name in 01-child-reports 02-child-writes 03-parent-reports 04-parent-harvests 05-parent-waits; do
  used=$(grep -cF "rule $name" /tmp/stub/log || true)
  [ "$used" = 1 ] || fail "rule $name answered $used requests, not 1"
done
head1=$head
$TOOL tools --repo /tmp/repo --head "$head1" > /tmp/parent-tools.jsonl

stage "each operation is recorded as the run_tool call the model made, and completed"
for id in toolu_spawn toolu_wait toolu_harvest toolu_async; do
  record=$(jq -c --arg id "$id" 'select(.id == $id)' /tmp/parent-tools.jsonl)
  [ -n "$record" ] || fail "$id has no tool record"
  [ "$(jq -r '.name' <<<"$record")" = run_tool ] || fail "$id is recorded as $(jq -r '.name' <<<"$record"), not run_tool"
  [ "$(jq -r '.status' <<<"$record")" = complete ] || fail "$id is $(jq -r '.status' <<<"$record"), not complete"
done

stage "spawn made the child, wait joined its relay, harvest applied its file"
$TOOL tool-observation --repo /tmp/repo --head "$head1" --id toolu_spawn > /tmp/spawn-observation.json
child=$(jq -r '.child // empty' /tmp/spawn-observation.json)
[ "${#child}" -gt 10 ] || fail "spawn returned no child id: $(cat /tmp/spawn-observation.json)"
[ "$child" = "$child_id" ] || fail "spawn made child $child, not the derived $child_id"
$TOOL children --repo /tmp/repo --head "$head1" > /tmp/children.jsonl
relay=$(jq -r --arg child "$child" 'select(.id == $child) | .relay' /tmp/children.jsonl)
assert_oid "$relay" "child relay"
wait_record=$(jq -c 'select(.id == "toolu_wait")' /tmp/parent-tools.jsonl)
[ "$(jq -r '.task' <<<"$wait_record")" = "$relay" ] || fail "wait did not join the child's relay"
$TOOL tool-observation --repo /tmp/repo --head "$head1" --id toolu_wait > /tmp/wait-observation.json
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
$TOOL tool-observation --repo /tmp/repo --head "$head1" --id toolu_async > /tmp/async-observation.json
grep -qF "$async_task" /tmp/async-observation.json || fail "run-async's answer omitted the task: $(cat /tmp/async-observation.json)"
assert_spine "$head1"
pass llm-subagent-path
