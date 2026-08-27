#!/bin/bash
# tests/max-tokens — a WORKER test, in dev/worker-test (it needs git).
#
# max_tokens continuation (design/agent-harness.md): when a round ends with
# stop_reason "max_tokens" the harness does NOT fail the turn — it appends the
# partial assistant content as a prefill and asks the model to resume,
# accumulating every partial into the one logical round. This drives a scripted
# stub through TWO truncations before an end_turn and asserts (a) the turn
# advanced, (b) its message is the concatenation of all three partials, and
# (c) each continuation request replayed the running prefill verbatim.
#
# All three are llm-step's, observed in what it sent the model and what it
# committed; the client only curried the turn and blocked on it.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "stage the stub and fixtures"
llm_test_setup

rm -rf /tmp/ws && mkdir -p /tmp/ws
echo "hello" > /tmp/ws/greeting.txt
caos put /tmp/ws /cas/ws >/dev/null || fail "publishing the workspace"
ws=$(caos hash /cas/ws)

stage "script the stub: two truncations, then end_turn"
# response_text joins text blocks with a blank line, so the turn message is the
# three partials joined by "\n\n".
P1='[{"text":"Part one.","type":"text"}]'
P2='[{"text":"Part two.","type":"text"}]'
P3='[{"text":"Part three.","type":"text"}]'
mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"max_tokens"}' "$P1" > /tmp/stub/response-1.json
printf '{"content":%s,"stop_reason":"max_tokens"}' "$P2" > /tmp/stub/response-2.json
printf '{"content":%s,"stop_reason":"end_turn"}'   "$P3" > /tmp/stub/response-3.json
stub_pid=""
port=""
start_stub /tmp/stub stub_pid port

# NO TOOL IMAGES: llm-step's `.caos-expr` binds its own (std/llm-step/DEPS), so
# a caller says what the turn is, never which shell the agent greps with.
new_llm_conversation max-tokens "$port" "$ws" \
  "You are a coding agent operating on a git workspace."

stage "run the turn"
dispatch_turn "$ws" "write me a long answer"
turn=$(wait_turn) || {
  echo "--- stub log" >&2; cat /tmp/stub/log >&2 || true
  fail "the turn never reached a terminal event"
}
echo "  turn $turn" >&2

stage "the turn advanced and concatenated the three partials"
git merge-base --is-ancestor "$human" "$turn" \
  || fail "terminal event does not descend from the queued event"
[ "$(git show -s --format=%an "$turn")" = "caos-agent" ] || fail "turn author"
want=$(printf 'Part one.\n\nPart two.\n\nPart three.')
[ "$(git show -s --format=%B "$turn" | jq -r .content)" = "$want" ] \
  || fail "turn message is not the concatenation of the three partials"
[ "$(git rev-parse "$turn^{tree}")" = "$ws" ] || fail "toolless turn changed the tree"
echo "  ok: single-parent turn, message = the three partials joined" >&2

stage "each continuation replayed the running prefill verbatim"
grep -qF '"max_tokens":64000' /tmp/stub/request-1.json || fail "max_tokens not sent"
grep -qF '{"content":"write me a long answer","role":"user"}]' /tmp/stub/request-1.json \
  || fail "round 1 user message wrong"
# Request 2 (first continuation) ends with round 1's partial prefilled.
grep -qF "\"content\":$P1,\"role\":\"assistant\"}]" /tmp/stub/request-2.json \
  || fail "round-1 partial not prefilled as the trailing assistant message in request 2"
# Request 3 (second continuation) carries BOTH prior partials as prefill.
grep -qF "\"content\":$P1,\"role\":\"assistant\"}" /tmp/stub/request-3.json \
  || fail "round-1 partial missing from request 3"
grep -qF "\"content\":$P2,\"role\":\"assistant\"}]" /tmp/stub/request-3.json \
  || fail "round-2 partial not the trailing assistant message in request 3"
[ ! -f /tmp/stub/request-4.json ] || fail "unexpected fourth request (turn should have ended)"
echo "  ok: two continuations, each prefilling the accumulated response" >&2

stage "done"
printf 'max-tokens: ALL PASS\n' > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
