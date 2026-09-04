#!/bin/bash
# shellcheck disable=SC1091,SC2034,SC2154
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "stage the stub and fixtures"
llm_test_setup

rm -rf /tmp/ws
mkdir -p /tmp/ws
echo "hello" > /tmp/ws/greeting.txt
ws=$(publish_tree /tmp/ws /cas/ws "publishing the workspace")

P1='[{"text":"Part one.","type":"text"}]'
P2='[{"text":"Part two.","type":"text"}]'
P3='[{"text":"Part three.","type":"text"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"max_tokens"}' "$P1" > /tmp/stub/response-1.json
printf '{"content":%s,"stop_reason":"max_tokens"}' "$P2" > /tmp/stub/response-2.json
printf '{"content":%s,"stop_reason":"end_turn"}' "$P3" > /tmp/stub/response-3.json
start_stub /tmp/stub

new_llm_conversation max-tokens "$STUB_PORT" "$ws" \
  "You are a coding agent operating on a git workspace."
dispatch_turn "write me a long answer"
wait_turn || fail "the max-tokens turn never reached a terminal head"

want=$(printf 'Part one.\n\nPart two.\n\nPart three.')
assistant_text=""
while read -r _ role _ _ encoded; do
  if [ "$role" = assistant ]; then assistant_text=$(printf '%s' "$encoded" | jq -r .); fi
done < <($TOOL transcript --repo /tmp/repo --head "$head")
[ "$assistant_text" = "$want" ] \
  || fail "terminal assistant entry did not concatenate all partials"
[ "$(workspace_commit "$head")" = "$base" ] || fail "toolless turn changed main"

grep -qF '"max_tokens":64000' /tmp/stub/request-1.json || fail "max_tokens not sent"
grep -qF '{"content":"write me a long answer","role":"user"}]' /tmp/stub/request-1.json \
  || fail "round 1 user message wrong"
grep -qF "\"content\":$P1,\"role\":\"assistant\"}]" /tmp/stub/request-2.json \
  || fail "round-1 partial not prefilled as the trailing assistant message in request 2"
grep -qF "\"content\":$P1,\"role\":\"assistant\"}" /tmp/stub/request-3.json \
  || fail "round-1 partial missing from request 3"
grep -qF "\"content\":$P2,\"role\":\"assistant\"}]" /tmp/stub/request-3.json \
  || fail "round-2 partial not the trailing assistant message in request 3"
[ ! -f /tmp/stub/request-4.json ] || fail "unexpected fourth request (turn should have ended)"

pass max-tokens
