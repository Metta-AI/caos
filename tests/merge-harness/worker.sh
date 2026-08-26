#!/bin/bash
# tests/merge-harness — a WORKER test, in dev/worker-test (it needs git).
#
# The merge tool through a canonical conversation. The canonical event head,
# rather than the llm-step result object, must retain the real merge ancestry —
# which is a claim about a ref, so this reads refs and needs no client.
set -euo pipefail

caos get /cas/args/common || { echo "FAIL: reading worker-common.sh" >&2; exit 1; }
# shellcheck disable=SC1090
source /cas/args/common

stage "workspace, feature, and scripted model"
llm_test_setup

rm -rf /tmp/ws && mkdir -p /tmp/ws
echo v1 > /tmp/ws/f.txt
caos put /tmp/ws /cas/ws >/dev/null || fail "publishing the workspace"
base_tree=$(caos hash /cas/ws)

rm -rf /tmp/feat && mkdir -p /tmp/feat
echo v1 > /tmp/feat/f.txt
echo hello > /tmp/feat/g.txt
caos put /tmp/feat /cas/feat >/dev/null || fail "publishing the feature tree"

R1='[{"id":"toolu_01","input":{"theirs":"feature"},"name":"merge","type":"tool_use"}]'
rm -rf /tmp/stub && mkdir -p /tmp/stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1" > /tmp/stub/response-1.json
printf '{"content":[{"text":"merged the feature branch","type":"text"}],"stop_reason":"end_turn"}' \
  > /tmp/stub/response-2.json
stub_pid=""
port=""
start_stub /tmp/stub stub_pid port

new_llm_conversation merge "$port" "$base_tree"

# The feature side branches off the conversation base, so the merge has real
# ancestry to preserve. Minted after `base` exists, and given a ref so the
# closure is anchored on the server for the merge worker to fetch.
feature=$(mint_commit /cas/feature "$(caos hash /cas/feat)" feature "$base")
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$feature" \
  || fail "fetching the feature commit back"
git push --quiet caos "$feature:refs/caos/req/$feature" || fail "pushing feature closure"

# CURRIED ONTO THE CONFIGURED llm-step, not passed to new_llm_conversation:
# currying takes an ArgTree and args and returns an ArgTree, so a caller adds
# what only it knows without the shared helper growing a parameter for it.
llm=$(caos curry --base:hash="$llm" --merge-refs="feature $feature") \
  || fail "currying the merge refs onto llm-step"

stage "dispatch merge turn"
dispatch_turn "$base_tree" "merge in the feature branch"
head=$(wait_turn) || {
  echo "--- stub log" >&2; cat /tmp/stub/log >&2 || true
  fail "the merge turn never reached a terminal event"
}

stage "event spine owns the merged workspace and ancestry"
git merge-base --is-ancestor "$feature" "$head" \
  || fail "feature is not reachable from canonical conversation head"
[ "$(git show "$head:g.txt")" = hello ] || fail "merged file g.txt missing"
[ "$(git show "$head:f.txt")" = v1 ] || fail "f.txt changed"

current=$head
count=0
roots=0
while [ "$current" != "$base" ]; do
  message=$(git show -s --format=%B "$current")
  jq -e 'type == "object" and (has("v") | not)' <<<"$message" >/dev/null \
    || fail "invalid event on conversation spine: $current"
  parent=$(git rev-parse "$current^1")
  declared_base=$(jq -r '.base // empty' <<<"$message")
  if [ -n "$declared_base" ]; then
    [ "$declared_base" = "$parent" ] \
      || fail "root event $current does not name its first parent"
    [ "$declared_base" = "$base" ] \
      || fail "root event $current does not name the expected workspace base"
    roots=$((roots + 1))
  elif [ "$parent" = "$base" ]; then
    fail "oldest event $current has no explicit base"
  fi
  current=$parent
  count=$((count + 1))
done
[ "$roots" -eq 1 ] || fail "event spine did not contain exactly one explicit base"
[ "$count" -ge 4 ] || fail "merge turn recorded too few events"

events=$(git log --first-parent --format=%B "$base..$head")
grep -Eq '"calls"[[:space:]]*:[[:space:]]*\[' <<<"$events" || fail "ordered call record is missing"
grep -Eq '"result"[[:space:]]*:[[:space:]]*\{' <<<"$events" || fail "tool result record is missing"
grep -qF '"id":"toolu_01"' <<<"$events" || fail "merge call was not recorded"
grep -qF '"tool_use_id":"toolu_01"' <<<"$events" || fail "merge result was not recorded"
grep -qF 'merged the feature branch' <<<"$events" || fail "assistant transcript missing"
terminal=$(git show -s --format=%B "$head")
grep -Eq '"status"[[:space:]]*:[[:space:]]*"idle"' <<<"$terminal" \
  || fail "turn did not become idle"
grep -qF "\"request\":\"$request\"" <<<"$terminal" \
  || fail "terminal event did not identify its request"

stage "done"
printf 'merge-harness: ALL PASS\n' > /tmp/report
cat /tmp/report >&2
caos put /tmp/report /cas/out
