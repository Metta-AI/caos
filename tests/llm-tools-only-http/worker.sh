#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
#
# tests/llm-tools-only-http — a tool call run with HTTP alone.
#
# The conversation and its admitted request are made as in tests/llm-tools-only,
# because opening a conversation is a prompt hook's job. From there a call
# needs ONE request: the configured step, the call's id, the tool and its
# arguments. The step declares the call on the conversation's active request
# itself and runs it, and the run's RESULT is the call's output:
#
#   POST /start/run   base:hash=<llm-step>  tools-only=<id>  tool=<name>  arguments=<json>
#   GET  /result/<ticket>   ->   ok tree <oid>   {status, observation, conversation}
#
# The caller names no request, no head and no conversation checkout, and reads
# nothing back from the conversation to learn what the tool said.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

server="${CAOS_SERVER_URL:?the runner supplies CAOS_SERVER_URL}"

# POST the parameters as a form body (curl encodes the values), and leave the
# status in $code and the body in $body.
post() { # <path> <name=value>...
  local path=$1
  shift
  local -a data=()
  local pair
  for pair in "$@"; do data+=(--data-urlencode "$pair"); done
  code=$(curl -sS -X POST -o /tmp/post.body -w '%{http_code}' "${data[@]}" "$server$path") \
    || fail "POST $path did not complete"
  body=$(cat /tmp/post.body)
}

# The step's parameters for one call. A fresh call id is a fresh expression, so
# a second call is never answered from the first's memo.
call_params() { # <call> <tool> <arguments>
  params=("base:hash=$llm" "tools-only=$1" "tool=$2" "arguments=$3" "salt=$SALT-$1")
}

# Read a result tree the step returned: its status and observation, and the
# head it left. The observation is the tool_result JSON the model would see.
read_result() { # <tree oid> <dir>
  caos get-hash "$1" "$2" || fail "reading the result tree $1"
  caos get "$2" || fail "expanding the result tree $1"
  caos get "$2/status" && caos get "$2/observation" || fail "fetching the result files of $1"
  result_status=$(cat "$2/status")
  result_observation=$(cat "$2/observation")
  result_head=$(caos hash "$2/conversation")
  assert_oid "$result_head" "the result's conversation"
}

stage "workspace and a step with no model to call"
llm_test_setup

rm -rf /tmp/ws
mkdir -p /tmp/ws
echo "hello tools" > /tmp/ws/greeting.txt
mkdir -p /tmp/ws/tools/sh
printf 'curry --base:hash=%s\n' "$(caos hash /cas/args/bash-tool)" \
  > /tmp/ws/tools/sh/.caos-expr
ws=$(publish_tree /tmp/ws /cas/ws "publishing the workspace")

rm -rf /tmp/stub
mkdir -p /tmp/stub
start_stub /tmp/stub

new_llm_conversation tools-only-http "$STUB_PORT" "$ws" \
  "You are a coding agent operating on a git workspace."
admit_turn "read the greeting, then write a file"

stage "a call started with a tool name and arguments, and nothing declared"
call_params toolu_read read '{"source_tree":"main","file-path":"greeting.txt"}'
post /start/run "${params[@]}"
[ "$code" = 200 ] || fail "start: HTTP $code: $body"
ticket=$body
[ "${#ticket}" -eq 40 ] || fail "expected a ticket, got: $ticket"
state=pending
for ((attempt = 0; attempt < 600; attempt++)); do
  state=$(curl -sS "$server/result/$ticket") || fail "GET /result did not complete"
  if [ "$state" != pending ]; then break; fi
  sleep 0.5
done
case "$state" in
  "ok tree "*) ;;
  *) fail "expected 'ok tree <oid>', got: $state" ;;
esac
read_result "${state#ok tree }" /cas/read-result
[ "$result_status" = complete ] || fail "the read ended $result_status: $result_observation"
case "$result_observation" in
  *"hello tools"*) echo "  ok: the result carries the file's contents" >&2 ;;
  *) fail "the result's observation lacks the file: $result_observation" ;;
esac

stage "the conversation recorded what the result says"
head=$($TOOL fetch --repo /tmp/repo --ref "$conversation_ref") \
  || fail "fetching the conversation"
head=${head#head }
[ "$head" = "$result_head" ] \
  || fail "the result's conversation $result_head is not the ref's head $head"
$TOOL tool-observation --repo /tmp/repo --head "$head" --request "$request" \
  --round 0 --id toolu_read > /tmp/read.observation \
  || fail "the conversation recorded no observation for the call"
grep -qF "hello tools" /tmp/read.observation || fail "the recorded observation lacks the file"
request_status=$($TOOL request --repo /tmp/repo --head "$head" --id "$request" | jq -r .status)
[ "$request_status" = running ] || fail "the request is $request_status, not running"

stage "a second call, run to completion with one blocking request"
call_params toolu_write run_tool \
  '{"arguments":{"cmd":"echo written > out.txt"},"path":"main/tools/sh"}'
post /eval/run "${params[@]}"
[ "$code" = 200 ] || fail "eval/run: HTTP $code: $body"
case "$body" in
  "tree "*) ;;
  *) fail "expected 'tree <oid>', got: $body" ;;
esac
read_result "${body#tree }" /cas/write-result
[ "$result_status" = complete ] || fail "the write ended $result_status: $result_observation"
case "$result_observation" in
  *"exit: 0"*) echo "  ok: the bash call reports a successful exit" >&2 ;;
  *) fail "the write's observation lacks its exit: $result_observation" ;;
esac

stage "a call that fails is a value, not a failed run"
call_params toolu_missing read '{"source_tree":"main","file-path":"no-such-file.txt"}'
post /eval/run "${params[@]}"
[ "$code" = 200 ] || fail "eval/run of a failing call: HTTP $code: $body"
read_result "${body#tree }" /cas/missing-result
# The tool failed, and says so in the observation the model would see; the run
# itself succeeded, so the caller reads it like any other.
case "$result_observation" in
  *is_error*true*) echo "  ok: the failure is in the observation" >&2 ;;
  *) fail "a missing file did not come back as an error observation: $result_observation" ;;
esac

stage "the same call again, as a new run, is declared once"
# A new salt makes a new ArgTree, so this RUNS the step again rather than
# replaying the memo; the call is already declared, and must not be twice.
params[4]="salt=$SALT-retry"
post /eval/run "${params[@]}"
[ "$code" = 200 ] || fail "repeat: HTTP $code: $body"
head=$($TOOL fetch --repo /tmp/repo --ref "$conversation_ref") || fail "fetching the conversation"
head=${head#head }
declared=$($TOOL tools --repo /tmp/repo --head "$head" --request "$request" \
  | jq -r --arg id toolu_missing 'select(.id == $id) | .id' | wc -l)
[ "$declared" -eq 1 ] || fail "toolu_missing is recorded $declared times"

stage "the request is still the harness's to end"
assert_spine "$head"
[ ! -f /tmp/stub/request-1.json ] || fail "a tools-only run called the model"

pass llm-tools-only-http
