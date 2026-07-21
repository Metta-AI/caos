#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# End-to-end agent-harness test (design/agent-harness.md) with NO real API
# calls: a scripted stub (llm-stub, on this host) plays the LLM, and llm-step's
# base_url points at it through the container network's host alias
# (host.containers.internal — podman defines it in every container; on plain
# docker this test would need an --add-host mapping).
#
# The script: round 1 asks bash to create out.txt; round 2 issues TWO tool
# calls in one response (a read, then a failing command); round 3 ends the
# turn. A second human turn then replays the whole first turn from the commit
# chain. Asserted with plain git on the fetched commits, and on the verbatim
# request bodies the stub recorded.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
mkcommit() { # <tree> <message> [parent] -> a commit minted with plain git
  local tree=$1 msg=$2 parent=${3:-}
  git -c user.email=test@caos -c user.name=caos \
    commit-tree "$tree" ${parent:+-p "$parent"} -m "$msg"
}

echo "== build the worker binaries and fixtures ==" >&2
# Prebuilt when the harness provides binaries (CAOS_BIN_DIR — the nested
# runner does), else built by the flake.
if [ -n "${CAOS_BIN_DIR:-}" ]; then
  cp "$CAOS_BIN_DIR/worker-bash-tool" bash-tool-bin
  cp "$CAOS_BIN_DIR/worker-llm-step" llm-step-bin
  stub_bin=$CAOS_BIN_DIR/llm-stub
else
  nix build "$CAOS_PROJECT#worker-bash-tool" -o bash-tool-out
  nix build "$CAOS_PROJECT#worker-llm-step" -o llm-step-out
  nix build "$CAOS_PROJECT#llm-stub" -o llm-stub-out
  cp -L bash-tool-out/bin/worker-bash-tool bash-tool-bin
  cp -L llm-step-out/bin/worker-llm-step llm-step-bin
  stub_bin=$(readlink -f llm-stub-out/bin/llm-stub)
  rm bash-tool-out llm-step-out llm-stub-out
fi

# The conversation's workspace: a small tree with a subdir that must survive
# the turn untouched.
mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
echo "You are a coding agent operating on a git workspace." > system.txt
commit "workspace + worker binaries"

# The conversation: a base commit over the ws tree, and the human turn above
# it (message = the user's text). Plain git objects — the harness's commits.
base=$(mkcommit "HEAD:ws" "base")
human1=$(mkcommit "HEAD:ws" "create out.txt containing hi, then confirm" "$base")

echo "== script the stub LLM (three rounds) ==" >&2
# Fixture JSON is written compact with keys pre-sorted exactly as serde_json
# re-serializes them, so byte-exact replay is assertable with grep -F.
R1_CONTENT='[{"signature":"sig-abc","thinking":"I should create the file.","type":"thinking"},{"text":"Creating out.txt.","type":"text"},{"id":"toolu_01","input":{"cmd":"echo hi > out.txt","paths":[]},"name":"bash","type":"tool_use"}]'
R2_CONTENT='[{"id":"toolu_02","input":{"cmd":"cat out.txt","paths":["out.txt"]},"name":"bash","type":"tool_use"},{"id":"toolu_03","input":{"cmd":"echo boom >&2; exit 3","paths":[]},"name":"bash","type":"tool_use"}]'
R3_TEXT="done: out.txt contains hi"
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1_CONTENT" > stub/response-1.json
printf '{"content":%s,"stop_reason":"tool_use"}' "$R2_CONTENT" > stub/response-2.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' "$R3_TEXT" > stub/response-3.json
printf '{"content":[{"text":"the workspace still holds out.txt","type":"text"}],"stop_reason":"end_turn"}' > stub/response-4.json

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

echo "== curry the workers and run the turn ==" >&2
conv="conv-$(printf '%s' "${CAOS_SALT:-dev}" | tr -cd '0-9a-zA-Z')"
bash_tool=$("$CAOS_CLI" curry /cas/std/runner -- --bin:@=bash-tool-bin)
# Workers reach the stub as host.containers.internal from the outer engine's
# container network; nested siblings share this job's netns (CAOS_STUB_HOST).
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
llm=$("$CAOS_CLI" curry /cas/std/runner -- --bin:@=llm-step-bin \
  --api_key=test-key --system:@=system.txt --bash_image="$bash_tool" \
  --model=test-model --base_url="http://$stub_host:$port" \
  --conversation="$conv")

"$CAOS_CLI" run "$llm" -- --head:commit="$human1" > turn.commit
turn=$(git hash-object -t commit --stdin < turn.commit)
git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$turn"

echo "== the turn commit: a merge of the human turn and the step chain ==" >&2
[ "$(git rev-parse "$turn^")" = "$human1" ] || fail "turn's first parent is not the human turn"
[ "$(git show -s --format=%an "$turn")" = "caos-agent" ] || fail "turn author"
[ "$(git show -s --format=%at "$turn")" -gt 0 ] || fail "turn has no wall-clock timestamp"
[ "$(git show -s --format=%s "$turn")" = "$R3_TEXT" ] || fail "turn message"
step3=$(git rev-parse "$turn^2") || fail "turn has no step chain"
step2=$(git rev-parse "$step3^")
step1=$(git rev-parse "$step2^")
[ "$(git rev-parse "$step1^")" = "$human1" ] || fail "step chain does not root at the human turn"
echo "  ok: [human, step3] parents; 3 steps rooted at the human turn" >&2

echo "== trees: workspace advanced, .caos only in step trees ==" >&2
[ "$(git show "$turn:out.txt")" = "hi" ] || fail "out.txt missing from the turn tree"
[ "$(git show "$turn:notes/todo.txt")" = "hello notes" ] || fail "untouched subtree lost"
git rev-parse -q --verify "$turn:.caos" >/dev/null && fail ".caos leaked into the turn tree"
for s in "$step1" "$step2" "$step3"; do
  git rev-parse -q --verify "$s:.caos/step.json" >/dev/null || fail "step $s has no step.json"
done
git rev-parse -q --verify "$step1:out.txt" >/dev/null && fail "step1 predates the write, but has out.txt"
[ "$(git show "$step2:out.txt")" = "hi" ] || fail "step2 tree misses the round-1 write"
git show "$step1:.caos/step.json" | grep -qF "$R1_CONTENT" || fail "step1 step.json lacks round-1 blocks"
git show "$step1:.caos/step.json" | grep -qF '"results":[]' || fail "step1 results not empty"
git show "$step3:.caos/step.json" | grep -qF '"is_error":true' || fail "step3 misses the failed call's result"
echo "  ok: out.txt in turn tree, step.json verbatim in step trees" >&2

echo "== the stub saw exact replays and single tool_result messages ==" >&2
grep -qF '"messages":[{"content":"create out.txt containing hi, then confirm","role":"user"}]' \
  stub/request-1.json || fail "round 1 messages wrong"
grep -qF '"model":"test-model"' stub/request-1.json || fail "model not sent"
grep -qF '"max_tokens":16000' stub/request-1.json || fail "max_tokens not sent"
grep -qF '"thinking":{"type":"adaptive"}' stub/request-1.json || fail "thinking not sent"
grep -qF '"cache_control":{"type":"ephemeral"}' stub/request-1.json || fail "cache_control not sent"
grep -qF '"name":"bash"' stub/request-1.json || fail "bash tool not registered"
# Round 2 replays round 1's assistant blocks byte-exactly and answers in one
# user message.
grep -qF "\"content\":$R1_CONTENT,\"role\":\"assistant\"" stub/request-2.json \
  || fail "round-1 blocks not replayed verbatim in round 2"
grep -qF '"tool_use_id":"toolu_01","type":"tool_result"}],"role":"user"}' stub/request-2.json \
  || fail "round-1 result not sent as the final user message"
grep -qF 'exit: 0' stub/request-2.json || fail "round-1 exit code not surfaced"
# Round 3: both round-2 results (the read and the failure) in ONE user message,
# adjacent blocks in one content array.
grep -qF "\"content\":$R2_CONTENT,\"role\":\"assistant\"" stub/request-3.json \
  || fail "round-2 blocks not replayed verbatim in round 3"
grep -qF '"tool_use_id":"toolu_02","type":"tool_result"},{"content"' stub/request-3.json \
  || fail "the two round-2 results are not in one user message"
grep -qF '"is_error":true' stub/request-3.json || fail "failing call not marked is_error"
grep -qF 'exit: 3' stub/request-3.json || fail "failing call's exit code missing"
grep -qF 'boom' stub/request-3.json || fail "failing call's stderr missing"
grep -qF 'stdout:\nhi' stub/request-3.json || fail "read call's stdout missing"
[ ! -f stub/request-4.json ] || fail "unexpected extra LLM round"
echo "  ok: verbatim replay, one user message per round's results" >&2

echo "== progress ref points at the newest step ==" >&2
adv=$(git ls-remote caos "refs/caos/conversations/$conv-progress" | cut -f1)
[ "$adv" = "$step3" ] || fail "progress ref is '$adv', want $step3"
echo "  ok: refs/caos/conversations/$conv-progress = step3" >&2

echo "== a second turn replays the first from the commit chain ==" >&2
human2=$(mkcommit "$turn^{tree}" "and now?" "$turn")
"$CAOS_CLI" run "$llm" -- --head:commit="$human2" > turn2.commit
turn2=$(git hash-object -t commit --stdin < turn2.commit)
git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$turn2"
[ "$(git rev-parse "$turn2^")" = "$human2" ] || fail "turn2's parent is not human2"
git rev-parse -q --verify "$turn2^2" >/dev/null && fail "toolless turn2 should have one parent"
[ "$(git show -s --format=%s "$turn2")" = "the workspace still holds out.txt" ] \
  || fail "turn2 message"
[ "$(git rev-parse "$turn2^{tree}")" = "$(git rev-parse "$turn^{tree}")" ] \
  || fail "toolless turn2 changed the tree"
grep -qF "\"content\":$R1_CONTENT,\"role\":\"assistant\"" stub/request-4.json \
  || fail "prior turn's round-1 blocks not replayed in turn 2"
grep -qF "\"content\":$R2_CONTENT,\"role\":\"assistant\"" stub/request-4.json \
  || fail "prior turn's round-2 blocks not replayed in turn 2"
grep -qF '{"content":"and now?","role":"user"}]' stub/request-4.json \
  || fail "turn2's user message missing/misplaced"
echo "  ok: full prior turn replayed from step.jsons; toolless turn2 is pure" >&2

echo "llm-step: ALL PASS" >&2
