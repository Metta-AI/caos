#!/usr/bin/env bash
# The merge tool through a canonical conversation, driven END TO END through the
# TESTED client ($CAOS_CLI). The property this pins down: after a real merge
# turn, the CANONICAL EVENT HEAD on the conversation ref — not the llm-step
# result object — is what owns the merged workspace AND the real git merge
# ancestry of the branch that was merged in.
#
# WHY THE TESTED CLI IS ESSENTIAL, NOT INCIDENTAL
#
# This subject cannot be checked by inspecting fixtures or running a script
# against $CAOS_PROJECT (the way tests/std-lint checks a static, generatable
# fact). Everything the assertions read into existence has to be PRODUCED by a
# genuine computation flowing through the tested client against the inner stack:
#
#   - `curry` composes the llm-step base with `--merge-refs`, so the turn even
#     KNOWS about the feature branch to merge.
#   - `prepare-request` must mint an exact content-addressed request Q (the
#     40-hex assertion), the identity the run and the terminal event pin to.
#   - `run` dispatches the whole merge turn through the inner server, runnerd
#     and redis: it must invoke the merge TOOL inside a real runner, perform an
#     actual git merge of `feature`, and publish an event spine to the canonical
#     conversation ref.
#
# The downstream assertions are precisely the things you CANNOT fabricate
# without that real round-trip having happened: `git merge-base --is-ancestor
# feature $head` (a true merge commit reachable from the canonical head), the
# merged file contents, the well-formed event spine with exactly one explicit
# base, the ordered `calls`/`result` records, the recorded merge `tool_use`/
# `tool_use_id`, the assistant transcript, and the idle terminal event naming
# its request. Each is an observable consequence of the tested client's
# merge-turn machinery working correctly; drive it with anything other than the
# tested client and you would be testing a different binary than the one under
# test. Hence every command below goes through $CAOS_CLI, as the harness
# (tests/lib/run-test.sh) requires.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
gc() { git -c user.email=test@caos -c user.name=caos "$@"; }
mkcommit() { # <tree> <message> [parent...] -> commit
  local tree=$1 message=$2
  shift 2
  local parents=() parent
  for parent in "$@"; do parents+=(-p "$parent"); done
  gc commit-tree "$tree" "${parents[@]}" -m "$message"
}

echo "== workspace, feature, and scripted model ==" >&2
"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry \
  || fail "resolving llm-stub"
stub_bin=/tmp/llm-stub-bin
install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"

mkdir ws
echo v1 > ws/f.txt
echo "You are a coding agent." > system.txt
git add -A
gc commit -qm fixtures
base_tree=$(git rev-parse HEAD:ws)
base=$(mkcommit "$base_tree" base)

mkdir feat
echo v1 > feat/f.txt
echo hello > feat/g.txt
git add feat
gc commit -qm feature-fixture
feature=$(mkcommit "$(git rev-parse HEAD:feat)" feature "$base")
git push --quiet caos "$feature:refs/caos/req/$feature" || fail "pushing feature closure"

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
  # The stub normally binds in a few milliseconds. Wait for the listener
  # instead of adding a fixed half-second delay to every test run.
  ready=0
  for _ in {1..400}; do
    if ! kill -0 "$stub_pid" 2>/dev/null; then break; fi
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then ready=1; break; fi
    sleep 0.005
  done
  if [ "$ready" = 1 ]; then break; fi
  kill "$stub_pid" 2>/dev/null || true
  wait "$stub_pid" 2>/dev/null || true
  stub_pid=""
done
[ -n "$stub_pid" ] || fail "could not start llm-stub: $(cat stub/log)"
trap 'kill "$stub_pid" 2>/dev/null || true' EXIT

echo "== dispatch merge turn ==" >&2
mkdir -p .caos-secrets
printf '.caos-secrets/\n' >> .git/info/exclude
printf '%s\n' \
  'name=anthropic-api-key' \
  'value=test-key' \
  'entropy=0123456789abcdef0123456789abcdef' \
  'reader=DEEP-DEPS/llm-step' \
  > .caos-secrets/anthropic-api-key
test_run_id="$(date +%s%N)-$$-$RANDOM"
conv="${test_run_id}-merge"
conversation_ref="refs/caos/v2/conversations/$conv/head"
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
llm=$("$CAOS_CLI" curry --base:@=DEEP-DEPS/llm-step \
  --system:@=system.txt \
  --merge-refs="feature $feature" \
  --model=test-model --base-url="http://$stub_host:$port" \
  --conversation="$conv")

user=$(mkcommit "$base_tree" \
  "{\"base\":\"$base\",\"author\":\"user\",\"content\":\"merge in the feature branch\"}" \
  "$base")
request=$("$CAOS_CLI" prepare-request --base:hash="$llm" --head:commit="$user")
[ "${#request}" -eq 40 ] && [[ "$request" =~ ^[0-9a-f]+$ ]] \
  || fail "prepared request is not exact Q: $request"
admitted=$(mkcommit "$base_tree" \
  "{\"request\":\"$request\",\"request_head\":\"$user\",\"status\":\"queued\"}" "$user")
git push --quiet caos "$admitted:$conversation_ref" || fail "publishing request admission"
"$CAOS_CLI" run --base:hash="$request" >/tmp/merge-result || fail "running merge turn"

advertised=$(git ls-remote --refs caos "$conversation_ref") \
  || fail "reading canonical conversation head"
head=${advertised%%$'\t'*}
[ -n "$head" ] || fail "canonical conversation head is absent"
git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$head" \
  || fail "fetching canonical conversation head"

echo "== event spine owns the merged workspace and ancestry ==" >&2
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

echo "merge-harness: ALL PASS" >&2
