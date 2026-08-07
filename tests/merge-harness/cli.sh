#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job (tests/lib/run-test.sh).
#
# The `merge` tool driven THROUGH the agent harness (SPEC "Merging and conflict
# resolution"), with a scripted stub for the LLM (like tests/llm-step): a turn
# whose one tool call is `merge --theirs=feature`. Asserts the whole stage-3
# wiring — the ref snapshot resolves `feature`, the git-bearing worker runs, its
# two-parent M becomes the workspace, and (the crux) `theirs` is REACHABLE from
# the turn commit, so the merge is a real merge in the conversation DAG.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
gc() { git -c user.email=test@caos -c user.name=caos "$@"; }
commit() { gc add -A && gc commit -qm "$1"; }
mkcommit() { # <tree> <message> [parent...] -> a commit minted with plain git
  local tree=$1 msg=$2; shift 2
  local ps=(); for p in "$@"; do ps+=(-p "$p"); done
  gc commit-tree "$tree" "${ps[@]}" -m "$msg"
}

echo "== stage bins and the workspace ==" >&2
stub_bin=$CAOS_BIN_DIR/llm-stub

# base workspace: one file. The human turn is text-only, so `ours` == base.
mkdir -p ws
echo v1 > ws/f.txt
echo "You are a coding agent." > system.txt
commit "workspace + bins"
base_tree=$(git rev-parse HEAD:ws)
base=$(mkcommit "$base_tree" base)
human=$(mkcommit "$base_tree" "merge in the feature branch" "$base")

# `feature`: shares base with the head and adds g.txt (a clean, non-conflicting
# merge). Push its closure to the server so the merge worker can fetch it —
# onto a content-addressed ref, exactly as the harness snapshot would.
mkdir -p feat
echo v1 > feat/f.txt
echo hello > feat/g.txt
gc add feat; gc commit -qm feat-scratch
feature=$(mkcommit "$(git rev-parse HEAD:feat)" feature "$base")
git push --quiet caos "$feature:refs/caos/req/$feature" || fail "pushing feature closure"

echo "== script the stub: round 1 merges, round 2 ends ==" >&2
R1='[{"id":"toolu_01","input":{"theirs":"feature"},"name":"merge","type":"tool_use"}]'
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1" > stub/response-1.json
printf '{"content":[{"text":"merged the feature branch","type":"text"}],"stop_reason":"end_turn"}' \
  > stub/response-2.json

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

echo "== curry the workers (merge-image + ref snapshot) and run the turn ==" >&2
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
bash_tool=$("$CAOS_CLI" curry /cas/std/bash-tool --)
llm=$("$CAOS_CLI" curry /cas/std/llm-step -- \
  --api-key=test-key --system:@=system.txt --bash-image="$bash_tool" \
  --merge-image=/cas/std/merge --merge-refs="feature $feature" \
  --model=test-model --base-url="http://$stub_host:$port")

"$CAOS_CLI" run "$llm" -- --head:commit="$human" > turn.commit
turn=$(git hash-object -t commit --stdin < turn.commit)
git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$turn" || fail "fetch turn"

echo "== the turn merged the feature branch, and theirs is reachable ==" >&2
[ "$(git rev-parse "$turn^")" = "$human" ] || fail "turn's first parent is not the human turn"
# The crux: the merge's M (and thus `theirs`) hangs off the turn by parent
# edges — a real merge in the DAG, not a detached snapshot.
git merge-base --is-ancestor "$feature" "$turn" \
  || fail "feature (theirs) is NOT reachable from the turn — the merge edge was lost"

# The workspace advanced to the merged tree: the feature's clean add is present.
[ "$(git show "$turn:g.txt")" = "hello" ] || fail "merged file g.txt missing from the turn tree"
[ "$(git show "$turn:f.txt")" = "v1" ] || fail "f.txt wrong in the turn tree"
# A clean merge leaves no conflict scaffolding, and .caos never leaks to a turn.
git rev-parse -q --verify "$turn:.caos" >/dev/null && fail ".caos leaked into the turn tree"
echo "  ok: feature reachable; g.txt merged in; no .caos in the turn tree" >&2

echo "merge-harness: ALL PASS" >&2
