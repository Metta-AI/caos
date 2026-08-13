#!/usr/bin/env bash
# End-to-end chat-v2 test against a scripted LLM. The single remote
# refs/caos/conversations/<id>/head ref is authoritative: every accepted event
# is on its first-parent spine, and remote work keeps advancing it after the
# submitting client disappears.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
mkcommit() { # <tree> <message> [parent]
  local tree=$1 msg=$2 parent=${3:-}
  git -c user.email=test@caos -c user.name=caos \
    commit-tree "$tree" ${parent:+-p "$parent"} -m "$msg"
}
remote_tip() { # <ref>
  local lines
  lines=$(git ls-remote --refs caos "$1")
  if [ -z "$lines" ]; then
    return 1
  fi
  if [ "$(printf '%s\n' "$lines" | wc -l)" -ne 1 ]; then
    fail "remote advertised $1 more than once"
  fi
  printf '%s\n' "${lines%%[[:space:]]*}"
}
capture_events() { # <head> <base> <output>
  local current=$1 base_commit=$2 output=$3 count=0 message
  : > "$output"
  while [ "$current" != "$base_commit" ]; do
    count=$((count + 1))
    if [ "$count" -gt 64 ]; then
      fail "event spine did not reach the conversation base"
    fi
    message=$(git show -s --format=%B "$current" | tr -d '\n')
    case "$message" in
      *'"v":2'*) ;;
      *) fail "non-v2 commit $current on the event spine: $message" ;;
    esac
    printf '%s\n' "$message" >> "$output"
    current=$(git rev-parse "$current^1")
  done
  EVENT_COUNT=$count
}

echo "== stage fixture and scripted LLM ==" >&2
"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry \
  || fail "resolving std/llm-stub"
stub_bin=/tmp/llm-stub-bin
install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"

mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
commit "chat-v2 fixture"
git config user.name tester
git config user.email tester@example.com
base=$(mkcommit "HEAD:ws" "base")

R1_CONTENT='[{"text":"Creating out.txt.","type":"text"},{"id":"toolu_01","input":{"cmd":"echo hi > out.txt","paths":[]},"name":"bash","type":"tool_use"}]'
T1_TEXT="done: out.txt contains hi"
T2_TEXT="the workspace still holds out.txt"
T3_TEXT="fresh conversation reply"
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1_CONTENT" > stub/response-1.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' "$T1_TEXT" > stub/response-2.json
# The third response is a FIFO. It lets the request reach llm-step and then
# blocks there while this test kills the submitting client.
mkfifo stub/response-3.json
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' "$T3_TEXT" > stub/response-4.json

stub_pid=""
for _ in 1 2 3 4 5; do
  port=$((20000 + RANDOM % 20000))
  "$stub_bin" "0.0.0.0:$port" "$PWD/stub" 2>stub/log &
  stub_pid=$!
  sleep 0.5
  if kill -0 "$stub_pid" 2>/dev/null; then
    break
  fi
  stub_pid=""
done
if [ -z "$stub_pid" ]; then
  fail "could not start llm-stub: $(cat stub/log)"
fi
trap 'kill "$stub_pid" 2>/dev/null || true' EXIT

conv="chat-$(printf '%s' "${CAOS_SALT:-dev}" | tr -cd '0-9a-zA-Z')"
ref="refs/caos/conversations/$conv/head"
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
opts=(--model test-model --base-url "http://$stub_host:$port")

echo "== failures publish no conversation ==" >&2
if env -u ANTHROPIC_API_KEY \
    "$CAOS_CLI" chat "$conv" -m "hello" --base "$base" "${opts[@]}" 2>key.err; then
  fail "chat succeeded without ANTHROPIC_API_KEY"
fi
grep -q "ANTHROPIC_API_KEY" key.err || fail "missing-key error is unclear"
if remote_tip "$ref" >/dev/null; then
  fail "missing-key failure created $ref"
fi
if [ -e stub/request-1.json ]; then
  fail "missing-key failure reached the LLM"
fi

export ANTHROPIC_API_KEY=test-key
mkdir -p bad/.caos
echo reserved > bad/.caos/marker
git add bad
git -c user.email=test@caos -c user.name=caos commit -qm "reserved base"
badbase=$(mkcommit "HEAD:bad" "bad base")
if "$CAOS_CLI" chat "bad-$conv" -m "hello" --base "$badbase" "${opts[@]}" 2>base.err; then
  fail "chat accepted a base with top-level .caos"
fi
grep -q "\.caos" base.err || fail "reserved-base error is unclear"
if remote_tip "refs/caos/conversations/bad-$conv/head" >/dev/null; then
  fail "reserved-base failure created a conversation"
fi
if [ -e stub/request-1.json ]; then
  fail "reserved-base failure reached the LLM"
fi

echo "== one tool-using turn is an append-only event spine ==" >&2
"$CAOS_CLI" chat "$conv" -m "create out.txt containing hi" \
  --base "$base" "${opts[@]}" >turn1.out
grep -qF "$T1_TEXT" turn1.out || fail "final response was not printed"

tip1=$(remote_tip "$ref") || fail "$ref was not created"
git fetch -q caos "$tip1"
[ "$(git show "$tip1:out.txt")" = "hi" ] || fail "tool mutation is absent from head"
[ "$(git show "$tip1:notes/todo.txt")" = "hello notes" ] || fail "untouched file was lost"
capture_events "$tip1" "$base" turn1.events
[ "$EVENT_COUNT" -ge 5 ] || fail "tool turn recorded only $EVENT_COUNT durable events"
grep -qF '"author":"user","content":"create out.txt containing hi"' turn1.events \
  || fail "user event is missing"
grep -q '"request":"[0-9a-f]\{40\}"' turn1.events || fail "exact request was not recorded"
tool_events=$(grep -cF 'toolu_01' turn1.events || true)
[ "$tool_events" -ge 2 ] || fail "tool call and result were not both recorded"
grep -qF '"name":"bash"' turn1.events || fail "tool name was not recorded"
grep -qF "$T1_TEXT" turn1.events || fail "assistant result event is missing"

git ls-remote --refs caos "refs/caos/conversations/$conv/*" > conversation.refs
[ "$(wc -l < conversation.refs)" -eq 1 ] || fail "conversation has side refs"
grep -q "[[:space:]]$ref$" conversation.refs || fail "the sole ref is not canonical head"

echo "== remote work survives loss of its submitting client ==" >&2
"$CAOS_CLI" chat "$conv" -m "and now?" "${opts[@]}" >turn2.out 2>turn2.err &
client_pid=$!
request_started=0
for _ in $(seq 1 150); do
  if [ -e stub/request-3.json ]; then
    request_started=1
    break
  fi
  if ! kill -0 "$client_pid" 2>/dev/null; then
    fail "client exited before the blocked request reached the LLM: $(cat turn2.err)"
  fi
  sleep 0.2
done
[ "$request_started" -eq 1 ] || fail "second request never reached the LLM"
kill "$client_pid" || fail "could not terminate the submitting client"
if wait "$client_pid"; then
  fail "terminated client unexpectedly exited successfully"
fi

# Release the already-running worker only after the client is gone.
printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' "$T2_TEXT" \
  > stub/response-3.json

recovered=0
for _ in $(seq 1 150); do
  tip2=$(remote_tip "$ref") || fail "$ref disappeared"
  git fetch -q caos "$tip2"
  history=$(git log --first-parent --format=%B "$tip2")
  if [[ "$history" == *"$T2_TEXT"* ]]; then
    recovered=1
    break
  fi
  sleep 0.2
done
[ "$recovered" -eq 1 ] || fail "worker did not finish after client disconnect"
[ "$tip2" != "$tip1" ] || fail "canonical head did not advance"
capture_events "$tip2" "$base" turn2.events
grep -qF '"author":"user","content":"and now?"' turn2.events \
  || fail "second user event is missing"
grep -qF "$T2_TEXT" turn2.events || fail "post-disconnect assistant event is missing"

# The second request was constructed entirely by replaying the event spine.
grep -qF '{"content":"create out.txt containing hi","role":"user"}' stub/request-3.json \
  || fail "first user message was not replayed"
grep -qF "\"content\":$R1_CONTENT,\"role\":\"assistant\"" stub/request-3.json \
  || fail "first turn's tool-calling response was not replayed"
grep -qF '{"content":"and now?","role":"user"}]' stub/request-3.json \
  || fail "second user message is missing or misplaced"

echo "== log replay and automatic naming use canonical refs ==" >&2
"$CAOS_CLI" chat "$conv" --log >log.out
grep -qF "user: create out.txt containing hi" log.out || fail "log misses first user event"
grep -qF "assistant: $T1_TEXT" log.out || fail "log misses first assistant event"
grep -qF "user: and now?" log.out || fail "log misses second user event"
grep -qF "assistant: $T2_TEXT" log.out || fail "log misses recovered assistant event"

"$CAOS_CLI" talk --new "fresh start" "${opts[@]}" >talk.out 2>talk.err
grep -qF "$T3_TEXT" talk.out || fail "auto-named conversation response is missing"
auto_ref=refs/caos/conversations/talk-1/head
auto_tip=$(remote_tip "$auto_ref") || fail "talk --new did not create $auto_ref"
git fetch -q caos "$auto_tip"
git show -s --format=%B "$auto_tip" | grep -qF '"v":2' \
  || fail "auto-named head is not a v2 event"
if git ls-remote --refs caos 'refs/caos/conversations/talk-1/from-*' | grep -q .; then
  fail "auto-named conversation created legacy refs"
fi

echo "chat-offline: ALL PASS" >&2
