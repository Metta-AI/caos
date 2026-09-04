#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

assert_parent_dates() {
  local conversation_head=$1 parents oid kind root="" last="" child_time="" parent_time
  parents=$($TOOL parents --repo /tmp/repo --head "$conversation_head" --validate) \
    || fail "walking the conversation spine"
  while read -r oid kind; do
    last=$kind
    if [ "$kind" = conversation.root ]; then root=$oid; fi
  done <<<"$parents"
  [ "$last" = conversation.root ] || fail "the conversation spine did not end at its root"

  while read -r oid parent_time; do
    if [ -n "$child_time" ] && [ "$child_time" != "$parent_time" ]; then
      fail "worker-minted commit before $oid did not inherit its parent's time"
    fi
    if [ "$oid" = "$root" ]; then break; fi
    child_time=$parent_time
  done < <(git log --first-parent --format='%H %ct' "$conversation_head")
}

stage "workspace and scripted model"
llm_test_setup

rm -rf /tmp/ws
mkdir -p /tmp/ws/notes
echo "hello notes" > /tmp/ws/notes/todo.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the workspace")

R1='[{"signature":"sig-abc","thinking":"I should create the file.","type":"thinking"},{"text":"Creating out.txt.","type":"text"},{"id":"toolu_01","input":{"cmd":"echo hi > out.txt","paths":[]},"name":"bash","type":"tool_use"}]'
R2='[{"id":"toolu_03","input":{"cmd":"echo boom >&2; exit 3","paths":[]},"name":"bash","type":"tool_use"}]'
EARLY_INTERJECTION_TEXT="also keep the notes subtree"
INTERJECTION_TEXT="one more thing before you finish"
STALE_T2_TEXT="the workspace still holds out.txt"
T2_TEXT="yes, I also saw your last message"

rm -rf /tmp/stub
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1" > /tmp/stub/response-1.json
printf '{"content":%s,"stop_reason":"tool_use"}' "$R2" > /tmp/stub/response-2.json
printf '%s\n' \
  '{"content":[{"text":"done: out.txt contains hi","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-3.json
mkfifo /tmp/stub/response-4.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$T2_TEXT" > /tmp/stub/response-5.json

start_stub /tmp/stub
new_llm_conversation llm-step "$STUB_PORT" "$ws" \
  "You are a coding agent operating on a git workspace."

stage "first turn: pre-dispatch interjection and durable tool records"
admit_turn "create out.txt containing hi, then confirm"
request1=$request
interject_output=$($TOOL interject --repo /tmp/repo --head "$head" --request "$request" \
  --actor racer --text "$EARLY_INTERJECTION_TEXT" --ref "$conversation_ref") \
  || fail "publishing the pre-dispatch interjection"
early_head=${interject_output%%$'\n'*}
early_head=${early_head#head }
early_message=${interject_output##*$'\n'}
early_message=${early_message#message }
head=$early_head
start_turn
wait_turn || fail "the first turn never reached a terminal head"
head1=$head

$TOOL request --repo /tmp/repo --head "$head1" --id "$request1" > /tmp/request1.record
jq -e --arg id "$early_message" '.interjections | index($id) != null' \
  /tmp/request1.record >/dev/null || fail "request record lost the pre-dispatch interjection"
$TOOL transcript --repo /tmp/repo --head "$head1" > /tmp/transcript1
roles=$(while read -r _ role _; do printf '%s\n' "$role"; done \
  < /tmp/transcript1 | paste -sd,)
case "$roles" in user,user,assistant*) ;;
  *) fail "transcript does not begin user, user-interjection, assistant: $roles" ;;
esac

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

workspace1=$(workspace_commit "$head1")
fetch_code "$workspace1" "fetching the first-turn workspace"
[ "$(git show "$workspace1:out.txt")" = hi ] || fail "out.txt is missing"
[ "$(git show "$workspace1:notes/todo.txt")" = "hello notes" ] \
  || fail "untouched subtree lost"
assert_parent_dates "$head1"

stage "second turn: terminal-race interjection"
admit_turn "and now?"
admitted2=$head
start_turn
wait_for_file /tmp/stub/request-4.json 150 \
  || fail "second turn never reached the blocked response"

interject_output=$($TOOL interject --repo /tmp/repo --request "$request" \
  --actor racer --text "$INTERJECTION_TEXT" --ref "$conversation_ref") \
  || fail "publishing the terminal-race interjection"
interjection=${interject_output%%$'\n'*}
interjection=${interjection#head }
running2=""
running_status=""
while read -r key value; do
  case "$key" in
    parent) running2=$value ;;
    status) running_status=$value ;;
  esac
done <<<"$interject_output"
assert_oid "$running2" "the running request head"
[ "$running_status" = running ] || fail "request is not running"
[ "$(git rev-parse "$running2^1")" = "$admitted2" ] \
  || fail "request.claim is not immediately after admission"
interjection_message=${interject_output##*$'\n'}
interjection_message=${interjection_message#message }
head=$interjection
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$STALE_T2_TEXT" > /tmp/stub/response-4.json

wait_turn || fail "the second turn never reached a terminal head"
head2=$head
[ "$(workspace_commit "$head2")" = "$workspace1" ] \
  || fail "toolless second turn changed main"
$TOOL transcript --repo /tmp/repo --head "$head2" > /tmp/transcript2
interjection_ordinal=""
answer_ordinal=""
while read -r ordinal role _actor message_id encoded; do
  if [ "$message_id" = "$interjection_message" ]; then interjection_ordinal=$ordinal; fi
  if [ "$role" = assistant ] && [ "$(printf '%s' "$encoded" | jq -r .)" = "$T2_TEXT" ]; then
    answer_ordinal=$ordinal
  fi
done < /tmp/transcript2
[ -n "$interjection_ordinal" ] || fail "racing interjection is absent from the transcript"
[ -n "$answer_ordinal" ] && [ "$answer_ordinal" -gt "$interjection_ordinal" ] \
  || fail "final assistant entry does not follow the racing interjection"
if transcript_text "$head2" | grep -qF "$STALE_T2_TEXT"; then
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
assert_parent_dates "$head2"

stage "workspace and blocked model response"
rm -rf /tmp/ws
mkdir -p /tmp/ws
echo "hello" > /tmp/ws/greeting.txt
ws=$(publish_tree /tmp/ws /cas/ws-interrupt "publishing the workspace")

mkfifo /tmp/stub/response-6.json
new_llm_conversation llm-interrupt "$STUB_PORT" "$ws" \
  "You are a coding agent operating on a git workspace."

stage "escape a running request at the model boundary"
dispatch_turn "this prompt was accidental"
request1=$request
wait_for_file /tmp/stub/request-6.json || fail "model request never arrived"

escape_output=$($TOOL escape --repo /tmp/repo --request "$request1" --ref "$conversation_ref") \
  || fail "publishing request.escape"
escape_head=""
escape_status=""
while read -r key value; do
  case "$key" in
    head) escape_head=$value ;;
    status) escape_status=$value ;;
  esac
done <<<"$escape_output"
assert_oid "$escape_head" "the escaped request head"
head=$escape_head
[ "$escape_status" = cancelling ] || fail "running escape did not cancel"

printf '%s\n' \
  '{"content":[{"text":"I had started this response.","type":"text"},{"id":"toolu_interrupted","input":{"file-path":"interrupted.txt","content":"must not run\n"},"name":"write","type":"tool_use"}],"stop_reason":"tool_use"}' \
  > /tmp/stub/response-6.json
wait_turn || fail "the interrupted turn never reached a terminal head"
head1=$head

stage "the request is interrupted before the stale response is admitted"
$TOOL request --repo /tmp/repo --head "$head1" --id "$request1" > /tmp/interrupted.request
jq -e '.status == "idle" and .interrupted == true' /tmp/interrupted.request >/dev/null \
  || fail "request did not end idle and interrupted"
$TOOL tools --repo /tmp/repo --head "$head1" --request "$request1" > /tmp/interrupted.tools
[ ! -s /tmp/interrupted.tools ] \
  || fail "post-escape model calls were admitted onto the conversation"
interrupted_transcript=$(transcript_text "$head1")
if grep -qF 'I had started this response.' <<<"$interrupted_transcript"; then
  fail "post-escape model response became canonical"
fi
[ "$(workspace_commit "$head1")" = "$base" ] \
  || fail "interrupted write changed main"
[ ! -e /tmp/stub/request-7.json ] || fail "escape allowed another model round"

stage "escape a queued request before it can be claimed"
new_llm_conversation llm-interrupt-queued "$STUB_PORT" - \
  "You are a coding agent operating on a git workspace." "$base" \
  "currying queued-interrupt llm-step"
admit_turn "cancel this before dispatch"
queued_request=$request
queued_admitted=$head
escape_output=$($TOOL escape --repo /tmp/repo --head "$head" --request "$queued_request" \
  --ref "$conversation_ref") || fail "escaping the queued request"
queued_escape=""
queued_status=""
queued_interrupted=""
while read -r key value; do
  case "$key" in
    head) queued_escape=$value ;;
    interrupted) queued_interrupted=$value ;;
    status) queued_status=$value ;;
  esac
done <<<"$escape_output"
assert_oid "$queued_escape" "the queued escape head"
head=$queued_escape
if [ "$queued_status" != idle ] || [ "$queued_interrupted" != true ]; then
  fail "queued escape was not terminal immediately"
fi
start_turn
sleep 0.2
[ "$(remote_tip "$conversation_ref")" = "$queued_escape" ] \
  || fail "dispatching an escaped queued request appended another event"
$TOOL parents --repo /tmp/repo --head "$queued_escape" > /tmp/queued.parents
if grep -q ' request.claim$' /tmp/queued.parents; then
  fail "queued escape acquired a worker claim"
fi
[ "$(git rev-parse "$queued_escape^1")" = "$queued_admitted" ] \
  || fail "queued request.escape did not append directly to admission"

pass llm-step
