#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# End-to-end `caos-cli chat` test (design/agent-harness.md, "Client") with NO
# real API calls: the scripted llm-stub plays the LLM exactly as in
# tests/llm-step. Covers, in order: the missing-API-key fail-fast, the
# reserved-`.caos`-base refusal, a two-turn conversation through the real verb
# (turn 1 creates refs/caos/conversations/<name>, turn 2 — message on stdin —
# advances it and replays turn 1's transcript to the stub), the progress
# output (a `$ <cmd>` tool-call line lands on stdout), and `chat --log`.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
mkcommit() { # <tree> <message> [parent] -> a commit minted with plain git
  local tree=$1 msg=$2 parent=${3:-}
  git -c user.email=test@caos -c user.name=caos \
    commit-tree "$tree" ${parent:+-p "$parent"} -m "$msg"
}

echo "== build the worker binaries and fixtures ==" >&2
nix build "$CAOS_PROJECT#worker-bash-tool" -o bash-tool-out
nix build "$CAOS_PROJECT#worker-llm-step" -o llm-step-out
nix build "$CAOS_PROJECT#llm-stub" -o llm-stub-out
cp -L bash-tool-out/bin/worker-bash-tool bash-tool-bin
cp -L llm-step-out/bin/worker-llm-step llm-step-bin
stub_bin=$(readlink -f llm-stub-out/bin/llm-stub)
rm bash-tool-out llm-step-out llm-stub-out

# The conversation's workspace, and the identity chat's human commits use.
mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
commit "workspace + worker binaries"
git config user.name tester
git config user.email tester@example.com

# The conversation base: a commit over just the ws tree (exercises --base —
# HEAD's tree here also carries the binaries and stub scripts).
base=$(mkcommit "HEAD:ws" "base")

echo "== script the stub LLM (two turns, three rounds) ==" >&2
R1_CONTENT='[{"text":"Creating out.txt.","type":"text"},{"id":"toolu_01","input":{"cmd":"echo hi > out.txt","paths":[]},"name":"bash","type":"tool_use"}]'
T1_TEXT="done: out.txt contains hi"
T2_TEXT="the workspace still holds out.txt"
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1_CONTENT" > stub/response-1.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' "$T1_TEXT" > stub/response-2.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' "$T2_TEXT" > stub/response-3.json

# Start the stub on a free port; workers reach this host as
# host.containers.internal on the container network.
stub_pid=""
for _ in 1 2 3 4 5; do
  port=$((20000 + RANDOM % 20000))
  "$stub_bin" "0.0.0.0:$port" "$PWD/stub" 2>stub/log &
  stub_pid=$!
  sleep 0.5
  kill -0 "$stub_pid" 2>/dev/null && break
  stub_pid=""
done
[ -n "$stub_pid" ] || fail "could not start llm-stub: $(cat stub/log)"
trap 'kill "$stub_pid" 2>/dev/null || true' EXIT

conv="chat-$(printf '%s' "${CAOS_SALT:-dev}" | tr -cd '0-9a-zA-Z')"
export CAOS_LLM_STEP_BIN=llm-step-bin CAOS_BASH_TOOL_BIN=bash-tool-bin
opts=(--model test-model --base-url "http://host.containers.internal:$port")

echo "== missing ANTHROPIC_API_KEY fails before minting anything ==" >&2
if env -u ANTHROPIC_API_KEY \
    "$CAOS_CLI" chat "$conv" -m "hello" --base "$base" "${opts[@]}" 2>key-err; then
  fail "chat succeeded without ANTHROPIC_API_KEY"
fi
grep -q "ANTHROPIC_API_KEY" key-err \
  || fail "no clear message about the missing key: $(cat key-err)"
git rev-parse -q --verify "refs/caos/conversations/$conv" >/dev/null \
  && fail "conversation ref exists after the key failure"
[ ! -f stub/request-1.json ] || fail "a request reached the stub despite the missing key"
echo "  ok: clean error, no ref, no request" >&2

export ANTHROPIC_API_KEY=test-key

echo "== a base tree with a reserved .caos entry is refused ==" >&2
mkdir -p caosdir/.caos
echo x > caosdir/.caos/marker
commit "tree with a .caos entry"
badbase=$(mkcommit "HEAD:caosdir" "bad base")
if "$CAOS_CLI" chat "bad-$conv" -m "hi" --base "$badbase" "${opts[@]}" 2>caos-err; then
  fail "chat accepted a base tree holding .caos"
fi
grep -q "\.caos" caos-err || fail "refusal does not mention .caos: $(cat caos-err)"
[ ! -f stub/request-1.json ] || fail "a request reached the stub despite the .caos refusal"
echo "  ok: refused with a .caos message" >&2

echo "== turn 1 creates the conversation ref ==" >&2
"$CAOS_CLI" chat "$conv" -m "create out.txt containing hi" --base "$base" "${opts[@]}" \
  > turn1.out
sed 's/^/  turn1| /' turn1.out >&2
turn1=$(git rev-parse -q --verify "refs/caos/conversations/$conv") \
  || fail "conversation ref not created"
human1=$(git rev-parse "$turn1^")
[ "$(git show -s --format=%s "$human1")" = "create out.txt containing hi" ] \
  || fail "human turn message"
[ "$(git show -s --format=%an "$human1")" = "tester" ] || fail "human turn author"
[ "$(git rev-parse "$human1^")" = "$base" ] || fail "human turn's parent is not the base"
[ "$(git show -s --format=%an "$turn1")" = "caos-agent" ] || fail "turn author"
[ "$(git show "$turn1:out.txt")" = "hi" ] || fail "out.txt missing from the turn tree"
[ "$(git show "$turn1:notes/todo.txt")" = "hello notes" ] || fail "untouched subtree lost"
echo "  ok: ref -> agent turn -> human turn -> base; tool ran" >&2

echo "== turn 1 printed progress and the response ==" >&2
grep -qF '$ echo hi > out.txt' turn1.out || fail "tool-call line not printed"
grep -qF "Creating out.txt." turn1.out || fail "step text not printed"
grep -qF "$T1_TEXT" turn1.out || fail "response text not printed"
grep -qF "[$conv " turn1.out || fail "conversation/short-hash line not printed"
[ "$(grep -cF "$T1_TEXT" turn1.out)" = 1 ] || fail "response text printed more than once"
echo "  ok: tool line, step text, response, hash line" >&2

echo "== turn 2 (message on stdin) advances the ref and replays turn 1 ==" >&2
echo "and now?" | "$CAOS_CLI" chat "$conv" "${opts[@]}" > turn2.out
sed 's/^/  turn2| /' turn2.out >&2
turn2=$(git rev-parse "refs/caos/conversations/$conv")
[ "$turn2" != "$turn1" ] || fail "conversation ref did not advance"
human2=$(git rev-parse "$turn2^")
[ "$(git rev-parse "$human2^")" = "$turn1" ] || fail "turn 2 does not chain onto turn 1"
[ "$(git show -s --format=%s "$human2")" = "and now?" ] || fail "turn 2's human message"
git rev-parse -q --verify "$turn2^2" >/dev/null && fail "toolless turn 2 should have one parent"
grep -qF "$T2_TEXT" turn2.out || fail "turn 2's response text not printed"
# The stub's third request replays the whole first turn from the commit chain.
grep -qF "\"content\":$R1_CONTENT,\"role\":\"assistant\"" stub/request-3.json \
  || fail "turn 1's assistant blocks not replayed in turn 2"
grep -qF '{"content":"create out.txt containing hi","role":"user"}' stub/request-3.json \
  || fail "turn 1's user message not replayed in turn 2"
grep -qF '{"content":"and now?","role":"user"}]' stub/request-3.json \
  || fail "turn 2's user message missing/misplaced"
[ ! -f stub/request-4.json ] || fail "unexpected extra LLM round"
echo "  ok: ref advanced; full turn-1 transcript replayed" >&2

echo "== --log prints the conversation ==" >&2
"$CAOS_CLI" chat "$conv" --log > log.out
sed 's/^/  log| /' log.out >&2
grep -qF "create out.txt containing hi" log.out || fail "--log misses the first human turn"
grep -qF "$T1_TEXT" log.out || fail "--log misses the first agent turn"
grep -qF "and now?" log.out || fail "--log misses the second human turn"
grep -qF "$T2_TEXT" log.out || fail "--log misses the second agent turn"
grep -qx "base" log.out && fail "--log printed the base commit"
echo "  ok: both turns, no base" >&2

echo "== talk (std worker curries): sticky pick continues $conv ==" >&2
# No CAOS_*_BIN overrides: the workers must resolve from the published std
# (refs/caos/std — build-builtins.sh publishes std/bash-tool and std/llm-step).
T3_TEXT="sticky turn reply"
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' "$T3_TEXT" > stub/response-4.json
env -u CAOS_LLM_STEP_BIN -u CAOS_BASH_TOOL_BIN \
  "$CAOS_CLI" talk "still there?" "${opts[@]}" > talk1.out 2>talk1.err
sed 's/^/  talk1| /' talk1.out >&2
grep -qF "[conversation $conv]" talk1.err \
  || fail "talk did not announce the sticky conversation: $(cat talk1.err)"
turn3=$(git rev-parse "refs/caos/conversations/$conv")
[ "$turn3" != "$turn2" ] || fail "talk did not advance the sticky conversation"
[ "$(git rev-parse "$turn3^^")" = "$turn2" ] || fail "talk turn does not chain onto turn 2"
grep -qF "$T3_TEXT" talk1.out || fail "talk's response text not printed"
grep -qF '{"content":"still there?","role":"user"}]' stub/request-4.json \
  || fail "talk's prompt missing from the request"
grep -qF '{"content":"and now?","role":"user"}' stub/request-4.json \
  || fail "earlier turns not replayed — talk continued the wrong conversation"
echo "  ok: std workers, sticky conversation continued and advanced" >&2

echo "== talk --new starts an auto-named conversation ==" >&2
T4_TEXT="fresh conversation reply"
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' "$T4_TEXT" > stub/response-5.json
env -u CAOS_LLM_STEP_BIN -u CAOS_BASH_TOOL_BIN \
  "$CAOS_CLI" talk --new "fresh start" "${opts[@]}" > talk2.out 2>talk2.err
sed 's/^/  talk2| /' talk2.out >&2
grep -qF "[conversation talk-1 — new]" talk2.err \
  || fail "talk --new did not announce a new talk-1: $(cat talk2.err)"
git rev-parse -q --verify refs/caos/conversations/talk-1 >/dev/null \
  || fail "talk --new did not create refs/caos/conversations/talk-1"
grep -qF "$T4_TEXT" talk2.out || fail "talk --new's response text not printed"
grep -qF '{"content":"and now?","role":"user"}' stub/request-5.json \
  && fail "old conversation replayed into the new one"
echo "  ok: talk-1 minted, no history carried over" >&2

echo "== talk argument-shape errors ==" >&2
if "$CAOS_CLI" talk "one" "two" 2>talk-err; then
  fail "talk accepted two positional prompts"
fi
grep -q "quote" talk-err || fail "extra-positional error not pointed: $(cat talk-err)"
if "$CAOS_CLI" talk "one" -m "two" 2>talk-err; then
  fail "talk accepted a positional prompt AND -m"
fi
grep -q "positionally" talk-err || fail "prompt-conflict error not pointed: $(cat talk-err)"
echo "  ok: pointed parse errors" >&2

echo "chat: ALL PASS" >&2
