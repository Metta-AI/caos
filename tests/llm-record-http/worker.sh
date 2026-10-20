#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
#
# tests/llm-record-http — what `caos mcp hook` used to write from the harness's
# checkout, written by the step over HTTP:
#
#   POST /eval/run   base:hash=<llm-step>  conversation=<id>  record=<prompt|stop|failure>
#                    message-id=<32 hex>  actor=<who>  text=<...>  [model=...]  [content:hash=<tree>]
#
# The first prompt CREATES the conversation, so nothing exists beforehand: no
# repo, no ref, no turn. Each event names its own message id, so repeating one
# writes nothing.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

server="${CAOS_SERVER_URL:?the runner supplies CAOS_SERVER_URL}"

# Record one event, leaving what the step answered in $status and the head it
# left in $recorded_head. A fresh salt per call keeps a call from being answered
# by the memo of one that looked the same.
calls=0
record() { # <kind> <message-id> <text> [extra name=value ...]
  local kind=$1 mid=$2 text=$3
  shift 3
  calls=$((calls + 1))
  local -a data=(
    --data-urlencode "base:hash=$LLM_TEST_LLM_STEP"
    --data-urlencode "conversation=$conv"
    --data-urlencode "record=$kind"
    --data-urlencode "message-id=$mid"
    --data-urlencode "actor=tester"
    --data-urlencode "text=$text"
    --data-urlencode "salt=$SALT-$calls"
  )
  local pair
  for pair in "$@"; do data+=(--data-urlencode "$pair"); done
  local code
  code=$(curl -sS -X POST -o /tmp/record.body -w '%{http_code}' "${data[@]}" "$server/eval/run") \
    || fail "POST /eval/run ($kind) did not complete"
  [ "$code" = 200 ] || fail "record $kind: HTTP $code: $(cat /tmp/record.body)"
  local tree
  tree=$(cat /tmp/record.body)
  case "$tree" in "tree "*) tree=${tree#tree } ;; *) fail "record $kind answered: $tree" ;; esac
  local dir=/cas/rec-$calls
  caos get-hash "$tree" "$dir" || fail "reading the result of $kind"
  caos get "$dir" || fail "expanding the result of $kind"
  caos get "$dir/status" || fail "fetching the status of $kind"
  status=$(cat "$dir/status")
  recorded_head=$(caos hash "$dir/conversation")
  assert_oid "$recorded_head" "the conversation after $kind"
}

# The conversation as the server holds it, in the checkout the test tool keeps.
subjects() { # -> the commit subjects, newest first, on one line
  local head
  head=$($TOOL fetch --repo /tmp/repo --ref "$ref") || fail "fetching the conversation"
  head=${head#head }
  git -C /tmp/repo log --format=%s "$head" | tr '\n' ' '
}

request_status() { # <request id>
  local head
  head=$($TOOL fetch --repo /tmp/repo --ref "$ref") || fail "fetching the conversation"
  head=${head#head }
  $TOOL request --repo /tmp/repo --head "$head" --id "$1" | jq -r .status
}

new_id() { od -An -N16 -tx1 /dev/urandom | tr -d ' \n'; }

stage "a conversation that does not exist yet"
llm_test_setup
conv="record-http-$(date +%s%N)-$$"
ref="refs/caos/v3/conversations/$(printf '%s' "$conv" | od -An -tx1 | tr -d ' \n')/head"
mkdir -p /tmp/repo
git -C /tmp/repo init -q 2>/dev/null || true
rm -rf /tmp/ws && mkdir -p /tmp/ws
echo "hello record" > /tmp/ws/greeting.txt
content=$(publish_tree /tmp/ws /cas/ws "publishing the content")

stage "the first prompt creates it, from the content it names"
p1=$(new_id)
record prompt "$p1" "read the greeting" "model=claude-code" "content:hash=$content"
[ "$status" = created ] || fail "the first prompt was '$status', not created"
case "$(subjects)" in
  "request.claim request.admit message.append conversation.root "*) echo "  ok: $(subjects)" >&2 ;;
  *) fail "unexpected history: $(subjects)" ;;
esac
first_request=$(git -C /tmp/repo log -1 --format=%H "$($TOOL fetch --repo /tmp/repo --ref "$ref" | cut -d' ' -f2)~2")
[ "$(request_status "$first_request")" = running ] || fail "the first request is not running"

stage "the same event again writes nothing"
before=$recorded_head
record prompt "$p1" "read the greeting" "model=claude-code" "content:hash=$content"
[ "$status" = recorded ] || fail "a repeated prompt was '$status'"
[ "$recorded_head" = "$before" ] || fail "a repeated prompt moved the head"

stage "a tool call on the new conversation"
code=$(curl -sS -X POST -o /tmp/call.body -w '%{http_code}' \
  --data-urlencode "base:hash=$LLM_TEST_LLM_STEP" --data-urlencode "conversation=$conv" \
  --data-urlencode "tools-only=toolu_one" --data-urlencode "tool=read" \
  --data-urlencode 'arguments={"file-path":"greeting.txt"}' \
  --data-urlencode "salt=$SALT-call" "$server/eval/run") || fail "the tool call did not complete"
[ "$code" = 200 ] || fail "tool call: HTTP $code: $(cat /tmp/call.body)"

stage "a stop closes the request and keeps the closing message"
s1=$(new_id)
record stop "$s1" "I read it."
[ "$status" = recorded ] || fail "the stop was '$status'"
[ "$(request_status "$first_request")" = idle ] || fail "the stop left the request $(request_status "$first_request")"
record stop "$s1" "I read it."
[ "$status" = recorded ] || fail "a repeated stop was '$status'"

stage "a stop with nothing open is not an error"
record stop "$(new_id)" "again"
[ "$status" = nothing ] || fail "a stop with no request was '$status'"

stage "a second prompt opens a request, and a failure closes it as failed"
p2=$(new_id)
record prompt "$p2" "do something else" "model=claude-code"
[ "$status" = recorded ] || fail "the second prompt was '$status'"
second_request=$(git -C /tmp/repo log -1 --format=%H "$($TOOL fetch --repo /tmp/repo --ref "$ref" | cut -d' ' -f2)~2")
[ "$second_request" != "$first_request" ] || fail "the second prompt reused the first request"
record failure "$(new_id)" "overloaded_error: try later"
[ "$status" = recorded ] || fail "the failure was '$status'"
[ "$(request_status "$second_request")" = failed ] \
  || fail "the failure left the request $(request_status "$second_request")"

stage "a stop for a conversation nobody created is refused"
code=$(curl -sS -X POST -o /tmp/none.body -w '%{http_code}' \
  --data-urlencode "base:hash=$LLM_TEST_LLM_STEP" --data-urlencode "conversation=no-such-$conv" \
  --data-urlencode "record=stop" --data-urlencode "message-id=$(new_id)" \
  --data-urlencode "actor=tester" --data-urlencode "text=x" "$server/eval/run") \
  || fail "the stop did not complete"
[ "$code" -ge 400 ] || fail "a stop with no conversation was accepted: $(cat /tmp/none.body)"

stage "the history is a valid spine"
head=$($TOOL fetch --repo /tmp/repo --ref "$ref")
assert_spine "${head#head }"

pass llm-record-http
