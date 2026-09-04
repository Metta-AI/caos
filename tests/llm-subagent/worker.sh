#!/bin/bash
# shellcheck disable=SC1090,SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
source /cas/args/common

stage "workspace and scripted parent/child model"
llm_test_setup

rm -rf /tmp/ws
mkdir -p /tmp/ws/notes
echo "hello notes" > /tmp/ws/notes/todo.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the workspace")

SUBAGENT_PROMPT="write child-output.txt with the delegated result"
TERMINAL_TEXT="subagent test round complete"
rm -rf /tmp/stub
mkdir -p /tmp/stub
printf '{"content":[{"id":"toolu_spawn","input":{"prompt":"%s"},"name":"spawn_agent","type":"tool_use"}],"stop_reason":"tool_use"}' \
  "$SUBAGENT_PROMPT" > /tmp/stub/response-1.json
mkfifo /tmp/stub/response-2.json /tmp/stub/response-3.json \
  /tmp/stub/response-4.json /tmp/stub/response-5.json
start_stub /tmp/stub

new_llm_conversation llm-subagent "$STUB_PORT" "$ws" \
  "You are a coding agent operating on a git workspace."

stage "atomic spawn and independent child work"
LLM_TEST_USERNAME=Alice
admit_turn "delegate a focused edit"
request1=$request
start_turn

# The parent and child race for the next model slot. The stub is sequential, so
# inspect each blocked request and release its FIFO with the response belonging
# to that conversation. Both conversations receive identical terminal text.
child_write_sent=0
parent_wait_sent=0
parent_terminal_sent=0
child_terminal_sent=0
for request_number in 2 3 4 5; do
  if ! wait_for_file "/tmp/stub/request-$request_number.json"; then
    dump_conversation "$conversation_ref" "$request1"
    fail "parent/child API request $request_number never arrived"
  fi
  if jq -e '.system | contains("You are a focused subagent")' \
    "/tmp/stub/request-$request_number.json" >/dev/null; then
    if [ "$child_write_sent" -eq 0 ]; then
      printf '%s\n' \
        '{"content":[{"id":"toolu_child_write","input":{"content":"written by child\n","file-path":"child-output.txt"},"name":"write","type":"tool_use"}],"stop_reason":"tool_use"}' \
        > "/tmp/stub/response-$request_number.json"
      child_write_sent=1
    else
      printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}\n' \
        "$TERMINAL_TEXT" > "/tmp/stub/response-$request_number.json"
      child_terminal_sent=1
    fi
  else
    if grep -qF '"tool_use_id":"toolu_wait"' "/tmp/stub/request-$request_number.json"; then
      printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}\n' \
        "$TERMINAL_TEXT" > "/tmp/stub/response-$request_number.json"
      parent_terminal_sent=1
    else
      wait_head=$(remote_tip "$conversation_ref") \
        || fail "parent wait response could not read the conversation head"
      wait_child=$($TOOL children --repo /tmp/repo --head "$wait_head" \
        | jq -r '.id')
      wait_child=${wait_child%%$'\n'*}
      [ -n "$wait_child" ] || fail "parent wait response could not find the child"
      printf '{"content":[{"id":"toolu_wait","input":{"child":"%s"},"name":"wait_agent","type":"tool_use"}],"stop_reason":"tool_use"}\n' \
        "$wait_child" > "/tmp/stub/response-$request_number.json"
      parent_wait_sent=1
    fi
  fi
done
[ "$child_write_sent" -eq 1 ] || fail "the child never received its write call"
[ "$child_terminal_sent" -eq 1 ] || fail "the child never received its terminal response"
[ "$parent_wait_sent" -eq 1 ] || fail "the parent never joined the child relay"
[ "$parent_terminal_sent" -eq 1 ] || fail "the parent never received its terminal response"

wait_turn || fail "the parent spawn turn never reached a terminal event"
head1=$head
$TOOL tools --repo /tmp/repo --head "$head1" --request "$request1" > /tmp/parent-tools.jsonl
spawn_record=$(jq -c 'select(.id == "toolu_spawn")' /tmp/parent-tools.jsonl)
[ -n "$spawn_record" ] || fail "spawn_agent has no tool record"
[ "$(jq -r '.name' <<<"$spawn_record")" = spawn_agent ] \
  || fail "spawn tool record has the wrong name"
[ "$(jq -r '.status' <<<"$spawn_record")" = complete ] \
  || fail "spawn tool record is not complete"
[ "$(jq -r '.task // "none"' <<<"$spawn_record")" = none ] \
  || fail "spawn tool record is not startless"

$TOOL tool-observation --repo /tmp/repo --head "$head1" --request "$request1" \
  --round 0 --id toolu_spawn > /tmp/spawn-observation.json
child=$(jq -r '.child // empty' /tmp/spawn-observation.json)
child_hex=${child#subagent-}
if [ "$child" = "$child_hex" ] || [ "${#child_hex}" -ne 64 ]; then
  fail "spawn returned invalid child id: $child"
fi
case "$child_hex" in *[!0-9a-f]*) fail "spawn returned invalid child id: $child" ;; esac
expected_child=$($TOOL child-id --parent "$conv" --request "$request1" --round 0 \
  --tool toolu_spawn) || fail "recomputing child id"
expected_child=${expected_child#child }
[ "$child" = "$expected_child" ] || fail "child id is not derived from the durable call"

$TOOL children --repo /tmp/repo --head "$head1" > /tmp/children.initial.jsonl
child_record=$(jq -c --arg child "$child" 'select(.id == $child)' /tmp/children.initial.jsonl)
[ -n "$child_record" ] || fail "parent has no child record"
initial_head=$(jq -r '.initial_head' <<<"$child_record")
child_request=$(jq -r '.request' <<<"$child_record")
relay=$(jq -r '.relay' <<<"$child_record")
assert_oid "$initial_head" "child initial head"
assert_oid "$child_request" "child request"
assert_oid "$relay" "child relay"
wait_record=$(jq -c 'select(.id == "toolu_wait")' /tmp/parent-tools.jsonl)
[ "$(jq -r '.status' <<<"$wait_record")" = complete ] \
  || fail "wait_agent did not complete"
[ "$(jq -r '.task' <<<"$wait_record")" = "$relay" ] \
  || fail "wait_agent did not join the child's relay"
$TOOL tool-observation --repo /tmp/repo --head "$head1" --request "$request1" \
  --round 1 --id toolu_wait > /tmp/wait-observation.json
[ "$(jq -r '.child' /tmp/wait-observation.json)" = "$child" ] \
  || fail "wait_agent observation names the wrong child"
[ "$(jq -r '.status' /tmp/wait-observation.json)" = completed ] \
  || fail "wait_agent observation is not completed"
[ "$(jq -r '.child' /tmp/spawn-observation.json)" = "$child" ] \
  || fail "spawn observation child disagrees"
[ "$(jq -r '.initial_head' /tmp/spawn-observation.json)" = "$initial_head" ] \
  || fail "spawn observation initial head disagrees"
[ "$(jq -r '.request' /tmp/spawn-observation.json)" = "$child_request" ] \
  || fail "spawn observation request disagrees"

stage "child root, ownership, workspace, and absence of membership"
spawn_commit=$($TOOL parents --repo /tmp/repo --head "$head1" \
  | while read -r oid kind; do if [ "$kind" = subagent.spawn ]; then printf '%s\n' "$oid"; fi; done)
assert_oid "$spawn_commit" "parent spawn commit"
pre_spawn=$(git rev-parse "$spawn_commit^1")
parent_main=$(jq -r '.input_workspace' <<<"$spawn_record")
[ "$(jq -r '.workspace_name' <<<"$spawn_record")" = main ] \
  || fail "spawn tool did not target main"
[ "$(jq -r '.initial_workspace' <<<"$child_record")" = "$parent_main" ] \
  || fail "child record did not pin main at spawn"

child_ref=$($TOOL ref --id "$child") || fail "forming child ref"
child_ref=${child_ref#ref }
child_tip_output=$($TOOL fetch --repo /tmp/repo --ref "$child_ref") \
  || fail "child has no canonical ref"
child_tip=${child_tip_output#head }
assert_oid "$child_tip" "child ref head"
git merge-base --is-ancestor "$initial_head" "$child_tip" \
  || fail "child ref does not descend from its recorded initial head"
initial_kinds=$($TOOL parents --repo /tmp/repo --head "$initial_head" --validate \
  | while read -r _ kind; do printf '%s\n' "$kind"; done | paste -sd,)
[ "$initial_kinds" = request.admit,message.append,conversation.root ] \
  || fail "child initial chain is not admit -> prompt -> root: $initial_kinds"
record "$initial_head" .caos/identity.json > /tmp/child-identity.json
[ "$(jq -r '.id' /tmp/child-identity.json)" = "$child" ] \
  || fail "child root identity id is wrong"
[ "$(jq -r '.owner.parent' /tmp/child-identity.json)" = "$conv" ] \
  || fail "child root owner parent is wrong"
[ "$(jq -r '.owner.parent_head' /tmp/child-identity.json)" = "$pre_spawn" ] \
  || fail "child root owner parent_head is not the pre-spawn head"
[ "$(workspace_commit "$initial_head")" = "$parent_main" ] \
  || fail "child initial main does not equal parent main at spawn"

child_key=$(printf '%s' "$child" | od -An -tx1 | tr -d ' \n')
membership=$(git ls-remote --refs caos \
  "refs/caos/v3/users/*/conversations/*/$child_key") \
  || fail "checking child membership refs"
[ -z "$membership" ] || fail "child unexpectedly has a user membership ref"

stage "relay terminal checkpoint"
terminal_record=""
seen=""
for _ in $(seq 1 300); do
  if remote=$(remote_tip "$conversation_ref") && [ "$remote" != "$seen" ]; then
    seen=$remote
    candidate=$remote
    terminal_record=$($TOOL children --repo /tmp/repo --head "$candidate" \
      | jq -c --arg child "$child" 'select(.id == $child and .status != "running")')
    if [ -n "$terminal_record" ]; then
      head1=$candidate
      break
    fi
  fi
  sleep 0.2
done
[ -n "$terminal_record" ] || fail "child terminal checkpoint never reached the parent"
[ "$(jq -r '.status' <<<"$terminal_record")" = completed ] \
  || fail "child terminal status is not completed"
terminal_head=$(jq -r '.terminal_head' <<<"$terminal_record")
assert_oid "$terminal_head" "child terminal head"
child_tip_output=$($TOOL fetch --repo /tmp/repo --ref "$child_ref") \
  || fail "fetching terminal child ref"
[ "${child_tip_output#head }" = "$terminal_head" ] \
  || fail "parent terminal checkpoint is not the child ref head"
child_main=$(jq -r '.child_workspaces.main.commit // empty' <<<"$terminal_record")
child_base=$(jq -r '.child_workspaces.main.initial // empty' <<<"$terminal_record")
assert_oid "$child_main" "terminal child main"
[ "$child_base" = "$parent_main" ] || fail "terminal child main lost its initial commit"
terminal_count=$($TOOL parents --repo /tmp/repo --head "$head1" \
  | while read -r _ kind; do if [ "$kind" = subagent.terminal ]; then echo x; fi; done \
  | wc -l | tr -d ' ')
[ "$terminal_count" -eq 1 ] || fail "parent has $terminal_count subagent.terminal commits"
fetch_code "$child_main" "fetching terminal child workspace"
[ "$(git show "$child_main:child-output.txt")" = "written by child" ] \
  || fail "child did not write child-output.txt"

stage "next parent turn harvests child code"
head=$head1
mkfifo /tmp/stub/response-6.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}\n' \
  "$TERMINAL_TEXT" > /tmp/stub/response-7.json
admit_turn "apply the child result"
request2=$request
start_turn
wait_for_file /tmp/stub/request-6.json || fail "harvest model request never arrived"
printf '{"content":[{"id":"toolu_harvest","input":{"child":"%s"},"name":"harvest_agent","type":"tool_use"}],"stop_reason":"tool_use"}\n' \
  "$child" > /tmp/stub/response-6.json
wait_turn || fail "the harvest turn never reached a terminal event"
head2=$head
parent_after=$(workspace_commit "$head2")
[ "$parent_after" != "$parent_main" ] || fail "harvest did not move parent main"
fetch_code "$parent_after" "fetching harvested parent workspace"
[ "$(git show "$parent_after:child-output.txt")" = "written by child" ] \
  || fail "harvested parent tree lacks the child's file"
$TOOL tool-observation --repo /tmp/repo --head "$head2" --request "$request2" \
  --round 0 --id toolu_harvest > /tmp/harvest-observation.json
resolution=$(jq -r '.kind // empty' /tmp/harvest-observation.json)
case "$resolution" in direct|merged) ;; *) fail "harvest resolution is $resolution" ;; esac

stage "spines and parent-only cost"
assert_spine "$head2"
assert_spine "$terminal_head"
spawn_count=0
terminal_count=0
apply_count=0
harvest_complete_count=0
while read -r _ kind harvest_present; do
  case "$kind" in
    subagent.spawn) spawn_count=$((spawn_count + 1)) ;;
    subagent.terminal) terminal_count=$((terminal_count + 1)) ;;
    subagent.apply) apply_count=$((apply_count + 1)) ;;
    tool.complete)
      if [ "$harvest_present" = true ]; then
        harvest_complete_count=$((harvest_complete_count + 1))
      fi
      ;;
    message.append|request.admit|request.claim|model.complete|request.terminal|tool.start) ;;
    *) fail "unexpected parent event $kind while child ran" ;;
  esac
done < <($TOOL parents --repo /tmp/repo --head "$head2" \
    --present-path ".caos/tools/$request2/0000/toolu_harvest.json" \
  | while read -r oid kind harvest_present; do
      if [ "$oid" = "$pre_spawn" ]; then break; fi
      printf '%s %s %s\n' "$oid" "$kind" "$harvest_present"
    done)
[ "$spawn_count" -eq 1 ] || fail "parent has $spawn_count spawn commits"
[ "$terminal_count" -eq 1 ] || fail "parent has $terminal_count terminal commits"
[ "$apply_count" -eq 1 ] || fail "parent has $apply_count apply commits"
[ "$harvest_complete_count" -eq 1 ] \
  || fail "parent has $harvest_complete_count harvest completion commits"

pass llm-subagent
