#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# End-to-end `caos-cli chat` test (design/agent-harness.md, "Client") with NO
# real API calls: the scripted llm-stub plays the LLM exactly as in
# tests/llm-step. Covers, in order: the missing-API-key fail-fast, the
# reserved-`.caos`-base refusal, a two-turn conversation through the real verb
# (turn 1 creates refs/caos/conversations/<name>/from-user, turn 2 — message on stdin —
# advances it and replays turn 1's transcript to the stub), the progress
# output (a `$ <cmd>` tool-call line lands on stdout), and `chat --log`.
#
# THE `chat` VERB ONLY. `talk` is tests/chat-talk and the agent's tool set is
# tests/chat-tools — three siblings sharing this bring-up rather than one
# script, because a turn costs about a second of real jobs and fifteen of them
# in a row made this the suite's critical path (design/faster-tests.md, "One
# test, three tests"). They must stay separable: if a stage here starts needing
# a `talk` turn or a tool call to have happened, it belongs in that sibling.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
# The stage clock. A test whose body is a dozen server round trips is profiled
# by WHERE its seconds went, and `seconds` in the record is one number for the
# lot: without this the answer to "why is this slow" starts by re-instrumenting
# the test (design/faster-tests.md).
T0=$(date +%s%3N)
stage() { echo "== $* == [+$(( $(date +%s%3N) - T0 ))ms]" >&2; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
mkcommit() { # <tree> <message> [parent] -> a commit minted with plain git
  local tree=$1 msg=$2 parent=${3:-}
  git -c user.email=test@caos -c user.name=caos \
    commit-tree "$tree" ${parent:+-p "$parent"} -m "$msg"
}

stage "stage the worker binaries and fixtures"
# The agent workers are std SOURCE entries, not host binaries: chat resolves
# llm-step/llm-call through the workspace's own DEPS and builds each via rustc
# (design/caos-expr.md, Phase 3). Only the LLM API stub is staged here, and only
# because the test needs a server it can point the workers at.
# The stub, from its std entry (std/llm-stub): a cargo `--cmd=build` result, so
# the executable is at bin/<name>. Copied out because materialized CAS content
# is read-only and owner-only — exec straight from /cas is "Permission denied".
"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry || fail "resolving std/llm-stub"
stub_bin=/tmp/llm-stub-bin
install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"

# The conversation's workspace, and the identity chat's human commits use.
mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
commit "workspace + worker binaries"
git config user.name tester
git config user.email tester@example.com

# The conversation base: a commit over just the ws tree (exercises --base —
# HEAD's tree here also carries the binaries and stub scripts).
base=$(mkcommit "HEAD:ws" "base")

stage "script the stub LLM (two turns, three rounds)"
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
  # Wait for the LISTENER, not for a fixed interval: a flat `sleep 0.5` here
  # was half a second of a test whose whole body is a few seconds, and the stub
  # binds in a few ms. Probing the port also tells the two failures apart — a
  # dead process (retry on another port) against one still coming up.
  ready=0
  for _ in {1..400}; do
    if ! kill -0 "$stub_pid" 2>/dev/null; then break; fi
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then ready=1; break; fi
    sleep 0.005
  done
  if [ "$ready" = 1 ]; then break; fi
  kill "$stub_pid" 2>/dev/null || true
  stub_pid=""
done
[ -n "$stub_pid" ] || fail "could not start llm-stub: $(cat stub/log)"
trap 'kill "$stub_pid" 2>/dev/null || true' EXIT

conv="chat-$(printf '%s' "${CAOS_SALT:-dev}" | tr -cd '0-9a-zA-Z')"
# Workers reach the stub as host.containers.internal from the outer engine's
# container network; nested siblings share this job's netns (CAOS_STUB_HOST).
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
opts=(--model test-model --base-url "http://$stub_host:$port")

stage "missing ANTHROPIC_API_KEY fails before minting anything"
if env -u ANTHROPIC_API_KEY \
    "$CAOS_CLI" chat "$conv" -m "hello" --base "$base" "${opts[@]}" 2>key-err; then
  fail "chat succeeded without ANTHROPIC_API_KEY"
fi
grep -q "ANTHROPIC_API_KEY" key-err \
  || fail "no clear message about the missing key: $(cat key-err)"
git rev-parse -q --verify "refs/caos/conversations/$conv/from-user" >/dev/null \
  && fail "conversation ref exists after the key failure"
[ ! -f stub/request-1.json ] || fail "a request reached the stub despite the missing key"
echo "  ok: clean error, no ref, no request" >&2

export ANTHROPIC_API_KEY=test-key

stage "a base tree with a reserved .caos entry is refused"
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

stage "turn 1 creates the conversation ref"
"$CAOS_CLI" chat "$conv" -m "create out.txt containing hi" --base "$base" "${opts[@]}" \
  > turn1.out
sed 's/^/  turn1| /' turn1.out >&2
turn1=$(git rev-parse -q --verify "refs/caos/conversations/$conv/from-user") \
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

stage "turn 1 printed progress and the response"
grep -qF '$ echo hi > out.txt' turn1.out || fail "tool-call line not printed"
grep -qF "Creating out.txt." turn1.out || fail "step text not printed"
grep -qF "$T1_TEXT" turn1.out || fail "response text not printed"
grep -qF "[$conv " turn1.out || fail "conversation/short-hash line not printed"
[ "$(grep -cF "$T1_TEXT" turn1.out)" = 1 ] || fail "response text printed more than once"
echo "  ok: tool line, step text, response, hash line" >&2

stage "turn 1 pushed the in-round status ref"
# The worker brackets each API attempt with refs/caos/conversations/<conv>/status — a blob
# "<human hash>\n<text>". The stub answers in ms so the client's 2s poll
# won't have printed it; assert the server-side ref and blob shape instead.
status_tip=$(git ls-remote caos "refs/caos/conversations/$conv/status" | cut -f1)
[ -n "$status_tip" ] || fail "no refs/caos/conversations/$conv/status on the server"
git fetch -q caos "$status_tip"
[ "$(git cat-file blob "$status_tip" | head -1)" = "$human1" ] \
  || fail "status blob not scoped to turn 1's human commit"
git cat-file blob "$status_tip" | sed -n 2p | grep -q "answered in" \
  || fail "status blob's last update is not the answered-in line: $(git cat-file blob "$status_tip")"
echo "  ok: status ref present, scoped to the turn, latency recorded" >&2

stage "turn 2 (message on stdin) advances the ref and replays turn 1"
echo "and now?" | "$CAOS_CLI" chat "$conv" "${opts[@]}" > turn2.out
sed 's/^/  turn2| /' turn2.out >&2
turn2=$(git rev-parse "refs/caos/conversations/$conv/from-user")
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

stage "--log prints the conversation"
"$CAOS_CLI" chat "$conv" --log > log.out
sed 's/^/  log| /' log.out >&2
grep -qF "create out.txt containing hi" log.out || fail "--log misses the first human turn"
grep -qF "$T1_TEXT" log.out || fail "--log misses the first agent turn"
grep -qF "and now?" log.out || fail "--log misses the second human turn"
grep -qF "$T2_TEXT" log.out || fail "--log misses the second agent turn"
grep -qx "base" log.out && fail "--log printed the base commit"
echo "  ok: both turns, no base" >&2

stage "done"
echo "chat-offline: ALL PASS" >&2
