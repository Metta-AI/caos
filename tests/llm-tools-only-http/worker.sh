#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
#
# tests/llm-tools-only-http — tests/llm-tools-only, with the step started over
# HTTP. The conversation is built and the call DECLARED exactly as there (the
# declaration is a conversation write, which no endpoint does yet); what changes
# is the hop that used to be `caos prepare-request` + `caos sub-run`:
#
#   POST /start/run?base:hash=<llm-step>&head:hash=<human>&run=<request>&tools-only=<call>
#   GET  /result/<ticket>
#
# `head:hash=` names a COMMIT. `eval_line` has no `:commit=`, and `:hash=` of a
# commit object binds it as a gitlink entry, which is what `--head:commit=` does.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

server="${CAOS_SERVER_URL:?the runner supplies CAOS_SERVER_URL}"

declare_call() {
  local call=$1 tool=$2 input=$3 output key value
  round=""
  output=$($TOOL declare --repo /tmp/repo --head "$head" --request "$request" \
    --actor tester --id "$call" --tool "$tool" --input "$input" \
    --ref "$conversation_ref") || fail "declaring $call"
  while read -r key value; do
    case "$key" in
      head) head=$value ;;
      round) round=$value ;;
    esac
  done <<<"$output"
  assert_oid "$head" "the declared conversation head"
  [ -n "$round" ] || fail "declare did not report a round"
}

# The step's parameters, as the query the endpoints take. A fresh call id makes
# a fresh expression, so a second call is never answered from the first's memo.
step_query() { # <call>
  printf 'base:hash=%s&head:hash=%s&run=%s&tools-only=%s&salt=%s' \
    "$llm" "$human" "$request" "$1" "$SALT-$1"
}

# Wait for the call's own record, which is what says it is done: the request
# stays running throughout, so there is nothing terminal to wait on.
await_call() { # <call>
  local call=$1 attempt status
  for ((attempt = 0; attempt < 600; attempt++)); do
    head=$($TOOL fetch --repo /tmp/repo --ref "$conversation_ref") \
      || fail "fetching the conversation after $call"
    head=${head#head }
    assert_oid "$head" "the conversation head after $call"
    status=$($TOOL tools --repo /tmp/repo --head "$head" --request "$request" \
      | jq -r --arg id "$call" 'select(.id == $id) | .status') || status=""
    case "$status" in
      complete) return 0 ;;
      failed | cancelled) fail "call $call ended $status" ;;
    esac
    sleep 0.2
  done
  fail "call $call never completed"
}

request_status() {
  $TOOL request --repo /tmp/repo --head "$head" --id "$request" | jq -r .status
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

stage "a read the harness declared, started over HTTP"
declare_call toolu_read read '{"source_tree":"main","file-path":"greeting.txt"}'
read_round=$round
code=$(curl -sS -X POST -o /tmp/start.body -w '%{http_code}' \
  "$server/start/run?$(step_query toolu_read)") || fail "POST /start/run did not complete"
ticket=$(tr -d '[:space:]' < /tmp/start.body)
[ "$code" = 200 ] || fail "start: HTTP $code: $(cat /tmp/start.body)"
[ "${#ticket}" -eq 40 ] || fail "expected a ticket, got: $ticket"
state=pending
for ((attempt = 0; attempt < 600; attempt++)); do
  state=$(curl -sS "$server/result/$ticket") || fail "GET /result did not complete"
  if [ "$state" != pending ]; then break; fi
  sleep 0.5
done
case "$state" in
  ok\ *) echo "  ok: $state" >&2 ;;
  *) fail "the step did not finish: $state" ;;
esac
await_call toolu_read
# A tools-only run's result is the conversation as it now stands: a tree whose
# `conversation` entry is the head (llm-step's `forward_result`). So a caller
# with only HTTP learns the new head from /result.
case "$state" in
  "ok tree "*) result_tree=${state#ok tree } ;;
  *) fail "expected 'ok tree <oid>', got: $state" ;;
esac
caos get-hash "$result_tree" /cas/step-result || fail "reading the step's result tree"
caos get /cas/step-result || fail "expanding the step's result tree"
[ "$(caos hash /cas/step-result/conversation)" = "$head" ] \
  || fail "the result's conversation is not the new head $head"
$TOOL tool-observation --repo /tmp/repo --head "$head" --request "$request" \
  --round "$read_round" --id toolu_read > /tmp/read.observation \
  || fail "the read call recorded no observation"
grep -qF "hello tools" /tmp/read.observation || fail "the read did not return the file"
[ "$(request_status)" = running ] \
  || fail "a tools-only run left the request $(request_status), not running"

stage "the observation, read back over HTTP through caos-conversation"
# The result above names the conversation, not what the tool said. The tool
# that reads a recorded conversation takes a tip hash and a call id, so an
# HTTP-only caller can get the output with one more request.
reader=$(caos hash /cas/args/reader)
assert_oid "$reader" "the caos-conversation image"
code=$(curl -sS -X POST -o /tmp/reader.body -w '%{http_code}' \
  "$server/eval/run?base:hash=$reader&hash=$head&call-id=toolu_read&salt=$SALT-reader") \
  || fail "POST /eval/run (caos-conversation) did not complete"
[ "$code" = 200 ] || fail "caos-conversation: HTTP $code: $(cat /tmp/reader.body)"
read -r reader_kind reader_oid < /tmp/reader.body
[ "$reader_kind" = blob ] || fail "expected the reader to answer a blob, got: $(cat /tmp/reader.body)"
# `GET /object/<oid>` is git's native `<type> <size>\0<content>`; the NUL goes.
curl -sS "$server/object/$reader_oid" | tr -d '\0' > /tmp/reader.text \
  || fail "fetching the reader's answer"
echo "  reader answered:" >&2
while IFS= read -r line; do echo "    $line" >&2; done < /tmp/reader.text
grep -qF "hello tools" /tmp/reader.text \
  || fail "the reader's answer does not contain the tool's output"
grep -qF "toolu_read" /tmp/reader.text \
  || fail "the reader's answer does not name the call"

stage "a second call, run to completion with one blocking request"
declare_call toolu_write run_tool \
  '{"arguments":{"cmd":"echo written > out.txt"},"path":"main/tools/sh"}'
write_round=$round
code=$(curl -sS -X POST -o /tmp/run.body -w '%{http_code}' \
  "$server/eval/run?$(step_query toolu_write)") || fail "POST /eval/run did not complete"
[ "$code" = 200 ] || fail "eval/run: HTTP $code: $(cat /tmp/run.body)"
echo "  ok: $(cat /tmp/run.body)" >&2
await_call toolu_write
$TOOL tool-observation --repo /tmp/repo --head "$head" --request "$request" \
  --round "$write_round" --id toolu_write > /tmp/write.observation \
  || fail "the bash call recorded no observation"
grep -qF 'exit: 0' /tmp/write.observation \
  || fail "the bash call did not report a successful exit"

stage "the request is still the harness's to end"
[ "$(request_status)" = running ] \
  || fail "the request ended without anyone terminating it"
assert_spine "$head"
[ ! -f /tmp/stub/request-1.json ] || fail "a tools-only run called the model"

pass llm-tools-only-http
