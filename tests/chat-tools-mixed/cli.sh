#!/usr/bin/env bash
# A mixed tool queue through chat: inline write, bash compute sub-run, then an
# inline edit. The ordering assertion is the point; it needs no prior turns.
set -euo pipefail
# The dependency is mounted only inside the test wrapper and exports globals.
# shellcheck disable=SC1091
source DEEP-DEPS/llm-test/common.sh

stage "workspace and scripted mixed-tool turn"
llm_test_setup
stub_host="${stub_host:?llm_test_setup did not set stub_host}"
mkdir ws
echo "fixture" > ws/original.txt
commit "workspace"
git config user.name tester
git config user.email tester@example.com
base=$(mkcommit "HEAD:ws" base)

MIXED_CALLS='[
 {"id":"tu_mw","input":{"file_path":"mix.txt","content":"hello"},"name":"write","type":"tool_use"},
 {"id":"tu_mb","input":{"cmd":"tr a-z A-Z < mix.txt > mix3.txt","paths":["mix.txt"]},"name":"bash","type":"tool_use"},
 {"id":"tu_me","input":{"file_path":"mix.txt","old_string":"hello","new_string":"world"},"name":"edit","type":"tool_use"}]'
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' \
  "$(printf '%s' "$MIXED_CALLS" | tr -d '\n')" > stub/response-1.json
printf '%s\n' \
  '{"content":[{"text":"mixed done","type":"text"}],"stop_reason":"end_turn"}' \
  > stub/response-2.json
stub_pid=""
port=""
start_stub stub stub_pid port
stub_pid="${stub_pid:?start_stub did not set stub_pid}"

test_run_id="$(date +%s%N)-$$-$RANDOM"
conv="${test_run_id}-tools-mixed"
conversation_ref="refs/caos/v2/conversations/$conv/head"
opts=(--model test-model --base-url "http://$stub_host:$port")

stage "inline write, bash, and inline edit stay ordered"
"$CAOS_CLI" chat "$conv" -m "mix inline and bash" \
  --base "$base" "${opts[@]}" > mixed.out
while IFS= read -r line; do echo "  mixed| $line" >&2; done < mixed.out
turn=$(remote_tip "$conversation_ref") || fail "mixed-tool conversation has no head"
git fetch -q caos "$turn"
[ "$(git show "$turn:mix.txt")" = "world" ] || fail "post-bash edit did not land"
[ "$(git show "$turn:mix3.txt")" = "HELLO" ] || fail "bash did not see inline write"
sequence=$(grep -o '"tool_use_id":"tu_m[wbe]"' stub/request-2.json \
  | grep -o 'tu_m[wbe]' | paste -sd,)
[ "$sequence" = "tu_mw,tu_mb,tu_me" ] \
  || fail "results missing or misordered: $sequence"
[ ! -f stub/request-3.json ] || fail "unexpected extra model round"

stage "done"
echo "chat-tools-mixed: ALL PASS" >&2
