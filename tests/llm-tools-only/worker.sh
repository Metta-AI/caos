#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

# Declare one call the way a harness that drives the model itself does, and
# leave it PENDING. `$head` and `$round` follow the declaration.
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

# Run the step for one declared call. A FRESH ARG TREE each time, naming the
# admitted request by `--run`: the request itself is one ArgTree, so a second
# call that reused it would be answered from the first's memo.
#
# `sub-run` starts the job and returns, so the call's own record is what says
# it is done -- the request stays running throughout, which is the whole point
# of the mode and leaves nothing terminal to wait on.
run_tools_only() {
  local call=$1 arg_tree attempt status
  arg_tree=$(caos prepare-request --base:hash="$llm" --head:commit="$human" \
    --run="$request" --tools-only="$call") \
    || fail "preparing the tools-only run for $call"
  caos sub-run "$arg_tree" >/dev/null || fail "dispatching the tools-only step for $call"
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
ws=$(publish_tree /tmp/ws /cas/ws "publishing the workspace")

# EMPTY on purpose: the stub has no scripted response, so a step that reaches
# the model fails rather than passing on a fixture. Tools-only never should.
rm -rf /tmp/stub
mkdir -p /tmp/stub
start_stub /tmp/stub

new_llm_conversation tools-only "$STUB_PORT" "$ws" \
  "You are a coding agent operating on a git workspace."
admit_turn "read the greeting, then write a file"

stage "a read the harness declared, run by the step"
declare_call toolu_read read '{"file-path":"greeting.txt"}'
read_round=$round
run_tools_only toolu_read
$TOOL tool-observation --repo /tmp/repo --head "$head" --request "$request" \
  --round "$read_round" --id toolu_read > /tmp/read.observation \
  || fail "the read call recorded no observation"
grep -qF "hello tools" /tmp/read.observation || fail "the read did not return the file"
[ "$(request_status)" = running ] \
  || fail "a tools-only run left the request $(request_status), not running"

stage "a second call, in its own run, mutating the workspace"
declare_call toolu_write bash '{"cmd":"echo written > out.txt","paths":[]}'
write_round=$round
[ "$write_round" != "$read_round" ] || fail "the second call declared the same round"
run_tools_only toolu_write
$TOOL tool-observation --repo /tmp/repo --head "$head" --request "$request" \
  --round "$write_round" --id toolu_write > /tmp/write.observation \
  || fail "the bash call recorded no observation"
grep -qF 'exit: 0' /tmp/write.observation \
  || fail "the bash call did not report a successful exit"

workspace=$(workspace_commit "$head")
fetch_code "$workspace" "fetching the workspace the tools produced"
[ "$(git show "$workspace:out.txt")" = written ] \
  || fail "the workspace does not carry the file the tool wrote"
[ "$(git show "$workspace:greeting.txt")" = "hello tools" ] \
  || fail "the workspace lost the file it started with"

stage "the request is still the harness's to end"
[ "$(request_status)" = running ] \
  || fail "the request ended without anyone terminating it"
assert_spine "$head"
[ ! -f /tmp/stub/request-1.json ] || fail "a tools-only run called the model"

pass llm-tools-only
