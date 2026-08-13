#!/usr/bin/env bash
# End-to-end v2 conversation test with a scripted LLM. The worker owns the
# canonical head after dispatch; its result object is deliberately ignored.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
gc() { git -c user.email=test@caos -c user.name=caos "$@"; }
mkcommit() { # <tree> <message> [parent] -> commit
  local tree=$1 message=$2 parent=${3:-}
  local parents=()
  if [ -n "$parent" ]; then parents=(-p "$parent"); fi
  gc commit-tree "$tree" "${parents[@]}" -m "$message"
}
remote_head() {
  local advertised
  advertised=$(git ls-remote --refs caos "$conversation_ref") \
    || fail "reading canonical conversation head"
  printf '%s\n' "${advertised%%$'\t'*}"
}
fetch_head() {
  local head
  head=$(remote_head)
  [ -n "$head" ] || fail "canonical conversation head is absent"
  git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$head" \
    || fail "fetching canonical conversation head"
  printf '%s\n' "$head"
}
assert_event_spine() { # <head> <stop>
  local current=$1 stop=$2 count=0 message
  while [ "$current" != "$stop" ]; do
    message=$(git show -s --format=%B "$current")
    grep -Eq '"v"[[:space:]]*:[[:space:]]*2' <<<"$message" \
      || fail "non-v2 commit on event spine: $current"
    current=$(git rev-parse "$current^")
    count=$((count + 1))
  done
  [ "$count" -ge 2 ] || fail "event spine is unexpectedly short"
}

echo "== workspace and scripted model ==" >&2
"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry \
  || fail "resolving llm-stub"
stub_bin=/tmp/llm-stub-bin
install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"

mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
printf '#!/bin/sh\necho hi\n' > ws/run.sh
chmod +x ws/run.sh
echo "You are a coding agent operating on a git workspace." > system.txt
git add -A
gc commit -qm fixtures
base=$(mkcommit "HEAD:ws" base)

R1='[{"signature":"sig-abc","thinking":"I should create the file.","type":"thinking"},{"text":"Creating out.txt.","type":"text"},{"id":"toolu_01","input":{"cmd":"echo hi > out.txt","paths":[]},"name":"bash","type":"tool_use"}]'
R2='[{"id":"toolu_02","input":{"cmd":"cat out.txt","paths":["out.txt"]},"name":"bash","type":"tool_use"},{"id":"toolu_03","input":{"cmd":"echo boom >&2; exit 3","paths":[]},"name":"bash","type":"tool_use"}]'
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1" > stub/response-1.json
printf '{"content":%s,"stop_reason":"tool_use"}' "$R2" > stub/response-2.json
printf '{"content":[{"text":"done: out.txt contains hi","type":"text"}],"stop_reason":"end_turn"}' \
  > stub/response-3.json
printf '{"content":[{"text":"the workspace still holds out.txt","type":"text"}],"stop_reason":"end_turn"}' \
  > stub/response-4.json

stub_pid=""
for _ in 1 2 3 4 5; do
  port=$((20000 + RANDOM % 20000))
  "$stub_bin" "0.0.0.0:$port" "$PWD/stub" 2>stub/log &
  stub_pid=$!
  sleep 0.5
  if kill -0 "$stub_pid" 2>/dev/null; then break; fi
  stub_pid=""
done
[ -n "$stub_pid" ] || fail "could not start llm-stub: $(cat stub/log)"
trap 'kill "$stub_pid" 2>/dev/null || true' EXIT

echo "== dispatch first turn ==" >&2
conv="conv-$(printf '%s' "${CAOS_SALT:-dev}" | tr -cd '0-9a-zA-Z')"
conversation_ref="refs/caos/conversations/$conv/head"
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
llm=$("$CAOS_CLI" curry DEEP-DEPS/llm-step -- \
  --api-key=test-key --system:@=system.txt \
  --model=test-model --base-url="http://$stub_host:$port" \
  --conversation="$conv")

user1=$(mkcommit "HEAD:ws" \
  '{"author":"user","content":"create out.txt containing hi, then confirm","status":"queued","v":2}' \
  "$base")
request1=$("$CAOS_CLI" curry "$llm" -- --head:commit="$user1")
running1=$(mkcommit "HEAD:ws" \
  "{\"request\":\"$request1\",\"status\":\"running\",\"v\":2}" \
  "$user1")
git push --quiet caos "$running1:$conversation_ref" || fail "publishing first request event"
"$CAOS_CLI" run "$request1" >/tmp/llm-step-result || fail "running first turn"

head1=$(fetch_head)
assert_event_spine "$head1" "$base"

echo "== durable events and workspace ==" >&2
events1=$(git log --first-parent --format=%B "$base..$head1")
grep -Eq '"response"[[:space:]]*:[[:space:]]*\[' <<<"$events1" || fail "model response record is missing"
grep -Eq '"calls"[[:space:]]*:[[:space:]]*\[' <<<"$events1" || fail "ordered call record is missing"
grep -Eq '"result"[[:space:]]*:[[:space:]]*\{' <<<"$events1" || fail "tool result record is missing"
grep -qF '"id":"toolu_01"' <<<"$events1" || fail "model tool call was not recorded"
grep -qF '"tool_use_id":"toolu_01"' <<<"$events1" || fail "tool result was not recorded"
grep -qF '"tool_use_id":"toolu_03"' <<<"$events1" || fail "second-round result was not recorded"
grep -qF '"is_error":true' <<<"$events1" || fail "failed tool result lost its error bit"
grep -qF 'done: out.txt contains hi' <<<"$events1" || fail "assistant transcript was not recorded"
terminal1=$(git show -s --format=%B "$head1")
grep -Eq '"status"[[:space:]]*:[[:space:]]*"idle"' <<<"$terminal1" \
  || fail "turn did not become idle"
grep -Eq '"request"[[:space:]]*:[[:space:]]*null' <<<"$terminal1" \
  || fail "turn did not clear its request"
[ "$(git show "$head1:out.txt")" = hi ] || fail "out.txt missing from canonical head"
[ "$(git show "$head1:notes/todo.txt")" = "hello notes" ] || fail "untouched subtree lost"
[ "$(git ls-tree "$head1" run.sh | cut -d' ' -f1)" = 100755 ] \
  || fail "executable mode changed"

echo "== model replay and second turn ==" >&2
grep -qF '"messages":[{"content":"create out.txt containing hi, then confirm","role":"user"}]' \
  stub/request-1.json || fail "first user message wrong"
grep -qF "\"content\":$R1,\"role\":\"assistant\"" stub/request-2.json \
  || fail "round-one response was not replayed verbatim"
grep -qF '"tool_use_id":"toolu_01","type":"tool_result"' stub/request-2.json \
  || fail "round-one result missing"
grep -qF "\"content\":$R2,\"role\":\"assistant\"" stub/request-3.json \
  || fail "round-two response was not replayed verbatim"
grep -qF 'exit: 3' stub/request-3.json || fail "failed command result missing"
[ ! -f stub/request-4.json ] || fail "unexpected extra model round"

tree1=$(git rev-parse "$head1^{tree}")
user2=$(mkcommit "$tree1" \
  '{"author":"user","content":"and now?","status":"queued","v":2}' "$head1")
request2=$("$CAOS_CLI" curry "$llm" -- --head:commit="$user2")
running2=$(mkcommit "$tree1" \
  "{\"request\":\"$request2\",\"status\":\"running\",\"v\":2}" "$user2")
git push --quiet caos "$running2:$conversation_ref" || fail "publishing second request event"
"$CAOS_CLI" run "$request2" >/tmp/llm-step-result-2 || fail "running second turn"

head2=$(fetch_head)
assert_event_spine "$head2" "$base"
[ "$(git rev-parse "$head2^{tree}")" = "$tree1" ] || fail "toolless turn changed the workspace"
terminal2=$(git show -s --format=%B "$head2")
grep -Eq '"status"[[:space:]]*:[[:space:]]*"idle"' <<<"$terminal2" \
  || fail "second turn did not become idle"
grep -Eq '"request"[[:space:]]*:[[:space:]]*null' <<<"$terminal2" \
  || fail "second turn retained its request"
grep -qF "\"content\":$R1,\"role\":\"assistant\"" stub/request-4.json \
  || fail "prior model response missing from second turn"
grep -qF "\"content\":$R2,\"role\":\"assistant\"" stub/request-4.json \
  || fail "prior second round missing from second turn"
grep -qF '{"content":"and now?","role":"user"}]' stub/request-4.json \
  || fail "second user message missing"

echo "llm-step: ALL PASS" >&2
