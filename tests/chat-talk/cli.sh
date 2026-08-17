#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# The `talk` verb (design/agent-harness.md, "Client"), with NO real API calls:
# the scripted llm-stub plays the LLM exactly as in tests/llm-step. Covers the
# explicit continuation (a `talk -c` replays the conversation's history),
# `--new` (creates a fresh named conversation and carries NO history over),
# and the two argument-shape errors, which never reach a server at all.
#
# Split out of tests/chat-offline, which drives the `chat` verb and is where
# this test's two seed turns come from in spirit — but not in fact: they are
# minted here, toolless and cheap, because sharing them would serialize the two
# tests behind one another and the point of the split was to stop paying for
# fifteen turns in a row (design/faster-tests.md, "One test, three tests").
# `talk` needs history to prove it CONTINUES something, so the seed is two
# turns, not one, and the second's message is what the replay assertions look
# for.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
T0=$(date +%s%3N)
stage() { echo "== $* == [+$(( $(date +%s%3N) - T0 ))ms]" >&2; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
mkcommit() { # <tree> <message> [parent] -> a commit minted with plain git
  local tree=$1 msg=$2 parent=${3:-}
  git -c user.email=test@caos -c user.name=caos \
    commit-tree "$tree" ${parent:+-p "$parent"} -m "$msg"
}
remote_tip() { # <ref>
  local lines
  lines=$(git ls-remote --refs caos "$1")
  [ -n "$lines" ] || return 1
  [ "${lines#*$'\n'}" = "$lines" ] || return 1
  printf '%s\n' "${lines%%[[:space:]]*}"
}

stage "stage the worker binaries and fixtures"
# The agent workers are std SOURCE entries, not host binaries: talk resolves
# llm-step through the workspace's own DEPS and builds it via rustc
# (design/caos-expr.md, Phase 3). Only the LLM API stub is staged here, and only
# because the test needs a server it can point the workers at.
# The stub, from its std entry (std/llm-stub): a cargo `--cmd=build` result, so
# the executable is at bin/<name>. Copied out because materialized CAS content
# is read-only and owner-only — exec straight from /cas is "Permission denied".
"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry || fail "resolving std/llm-stub"
stub_bin=/tmp/llm-stub-bin
install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"

# The conversation's workspace, and the identity talk's human commits use.
mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
commit "workspace + worker binaries"
git config user.name tester
git config user.email tester@example.com
export ANTHROPIC_API_KEY=test-key

# The conversation base: a commit over just the ws tree (HEAD's tree here also
# carries the binaries and stub scripts).
base=$(mkcommit "HEAD:ws" "base")

stage "script the stub LLM (two seed turns, then two talk turns)"
S1_TEXT="seeded the conversation"
S2_TEXT="the conversation has two turns"
T3_TEXT="sticky turn reply"
T4_TEXT="fresh conversation reply"
mkdir stub
# All four rounds are toolless end_turns: this test is about which conversation
# a `talk` lands in, not about what a turn can do — the tool set is
# tests/chat-tools. A tool call here would cost a sub-run per turn and prove
# nothing this file asserts.
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' "$S1_TEXT" > stub/response-1.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' "$S2_TEXT" > stub/response-2.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' "$T3_TEXT" > stub/response-3.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' "$T4_TEXT" > stub/response-4.json

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

test_run_id="$(date +%s%N)-$$-$RANDOM"
conv="${test_run_id}-talkseed"
# Workers reach the stub as host.containers.internal from the outer engine's
# container network; nested siblings share this job's netns (CAOS_STUB_HOST).
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
opts=(--model test-model --base-url "http://$stub_host:$port")

stage "seed a two-turn conversation for talk to continue"
"$CAOS_CLI" chat "$conv" -m "first message" --base "$base" "${opts[@]}" > seed1.out
sed 's/^/  seed1| /' seed1.out >&2
"$CAOS_CLI" chat "$conv" -m "and now?" "${opts[@]}" > seed2.out
sed 's/^/  seed2| /' seed2.out >&2
ref="refs/caos/v2/conversations/$conv/head"
turn2=$(remote_tip "$ref") || fail "seed conversation not created"
git fetch -q caos "$turn2"
echo "  ok: $conv has two turns" >&2

stage "talk (std worker curries): explicit pick continues $conv"
# The workers resolve from the workspace's declared deps and build from source.
"$CAOS_CLI" talk -c "$conv" "still there?" "${opts[@]}" > talk1.out 2>talk1.err
sed 's/^/  talk1| /' talk1.out >&2
grep -qF "[conversation $conv]" talk1.err \
  || fail "talk did not announce the sticky conversation: $(cat talk1.err)"
turn3=$(remote_tip "$ref") || fail "talk lost the sticky conversation"
git fetch -q caos "$turn3"
[ "$turn3" != "$turn2" ] || fail "talk did not advance the sticky conversation"
git merge-base --is-ancestor "$turn2" "$turn3" \
  || fail "talk turn does not chain onto turn 2"
grep -qF "$T3_TEXT" talk1.out || fail "talk's response text not printed"
grep -qF '{"content":"still there?","role":"user"}]' stub/request-3.json \
  || fail "talk's prompt missing from the request"
grep -qF '{"content":"and now?","role":"user"}' stub/request-3.json \
  || fail "earlier turns not replayed — talk continued the wrong conversation"
echo "  ok: std workers, selected conversation continued and advanced" >&2

fresh="${test_run_id}-talk-fresh"
stage "talk --new starts a named conversation"
"$CAOS_CLI" talk --new -c "$fresh" "fresh start" "${opts[@]}" > talk2.out 2>talk2.err
sed 's/^/  talk2| /' talk2.out >&2
grep -qF "[conversation $fresh — new]" talk2.err \
  || fail "talk --new did not announce $fresh: $(cat talk2.err)"
remote_tip "refs/caos/v2/conversations/$fresh/head" >/dev/null \
  || fail "talk --new did not create refs/caos/v2/conversations/$fresh/head"
grep -qF "$T4_TEXT" talk2.out || fail "talk --new's response text not printed"
grep -qF '{"content":"and now?","role":"user"}' stub/request-4.json \
  && fail "old conversation replayed into the new one"
[ ! -f stub/request-5.json ] || fail "unexpected extra LLM round"
echo "  ok: $fresh minted, no history carried over" >&2

stage "talk argument-shape errors"
if "$CAOS_CLI" talk "one" "two" 2>talk-err; then
  fail "talk accepted two positional prompts"
fi
grep -q "quote" talk-err || fail "extra-positional error not pointed: $(cat talk-err)"
if "$CAOS_CLI" talk "one" -m "two" 2>talk-err; then
  fail "talk accepted a positional prompt AND -m"
fi
grep -q "positionally" talk-err || fail "prompt-conflict error not pointed: $(cat talk-err)"
echo "  ok: pointed parse errors" >&2

stage "done"
echo "chat-talk: ALL PASS" >&2
