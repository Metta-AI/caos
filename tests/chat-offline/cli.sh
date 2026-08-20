#!/usr/bin/env bash
# End-to-end line-client test against a scripted LLM. Request preparation must
# fail before admission, and an admitted request must keep advancing its
# canonical conversation after the submitting client disappears. The completed
# closure must then be readable by a completely fresh client.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
mkcommit() { # <tree> <message> [parent]
  local tree=$1 message=$2 parent=${3:-}
  local parents=()
  if [ -n "$parent" ]; then parents=(-p "$parent"); fi
  git -c user.email=test@caos -c user.name=caos \
    commit-tree "$tree" "${parents[@]}" -m "$message"
}
remote_tip() { # <ref>
  local lines
  lines=$(git ls-remote --refs caos "$1") || return 1
  [ -n "$lines" ] || return 1
  [ "${lines#*$'\n'}" = "$lines" ] || fail "remote advertised $1 more than once"
  printf '%s\n' "${lines%%[[:space:]]*}"
}
capture_events() { # <head> <base> <output>
  local current=$1 base_commit=$2 output=$3 count=0 message parent declared_base roots=0
  : > "$output"
  while [ "$current" != "$base_commit" ]; do
    count=$((count + 1))
    [ "$count" -le 64 ] || fail "event spine did not reach the conversation base"
    message=$(git show -s --format=%B "$current" | tr -d '\n')
    jq -e 'type == "object" and (has("v") | not)' <<<"$message" >/dev/null \
      || fail "invalid event $current on the conversation spine: $message"
    printf '%s\n' "$message" >> "$output"
    parent=$(git rev-parse "$current^1")
    declared_base=$(jq -r '.base // empty' <<<"$message")
    if [ -n "$declared_base" ]; then
      [ "$declared_base" = "$parent" ] \
        || fail "root event $current does not name its first parent as base"
      [ "$declared_base" = "$base_commit" ] \
        || fail "root event $current names the wrong conversation base"
      roots=$((roots + 1))
    elif [ "$parent" = "$base_commit" ]; then
      fail "oldest event $current has no explicit base"
    fi
    current=$parent
  done
  [ "$roots" -eq 1 ] || fail "event spine did not contain exactly one explicit base"
  EVENT_COUNT=$count
}

echo "== stage fixture and scripted LLM ==" >&2
"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry \
  || fail "resolving std/llm-stub"
stub_bin=/tmp/llm-stub-bin
install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"

mkdir -p ws/notes
echo "hello notes" > ws/notes/todo.txt
commit "chat fixture"
git config user.name tester
git config user.email tester@example.com
base=$(mkcommit "HEAD:ws" "base")

TALK_TEXT="remote conversation reply"
mkdir stub
mkfifo stub/response-1.json

stub_pid=""
client_pid=""
cleanup() {
  if [ -n "$client_pid" ]; then kill "$client_pid" 2>/dev/null || true; fi
  if [ -n "$stub_pid" ]; then kill "$stub_pid" 2>/dev/null || true; fi
}
trap cleanup EXIT

for _ in 1 2 3 4 5; do
  port=$((20000 + RANDOM % 20000))
  "$stub_bin" "0.0.0.0:$port" "$PWD/stub" 2>stub/log &
  stub_pid=$!
  ready=0
  for _ in {1..400}; do
    if ! kill -0 "$stub_pid" 2>/dev/null; then break; fi
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
      ready=1
      break
    fi
    sleep 0.005
  done
  if [ "$ready" = 1 ]; then break; fi
  kill "$stub_pid" 2>/dev/null || true
  wait "$stub_pid" 2>/dev/null || true
  stub_pid=""
done
[ -n "$stub_pid" ] || fail "could not start llm-stub: $(cat stub/log)"

test_id="$(date +%s%N)-$$-$RANDOM"
conv="${test_id}-talk"
ref="refs/caos/v2/conversations/$conv/head"
queued_conv="${test_id}-queued-chat"
queued_ref="refs/caos/v2/conversations/$queued_conv/head"
bad_conv="${test_id}-bad-chat"
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
opts=(--model test-model --base-url "http://$stub_host:$port")

echo "== request preparation fails before admission ==" >&2
if "$CAOS_CLI" chat "$queued_conv" -m "hello" --base "$base" "${opts[@]}" 2>key.err; then
  fail "chat succeeded without its model secret"
fi
grep -q "anthropic-api-key" key.err || fail "missing-key error is unclear"
grep -qF '.caos-secrets/anthropic-api-key' key.err \
  || fail "missing-key error does not name the setup file"
grep -qF 'reader=DEEP-DEPS/llm-step' key.err \
  || fail "missing-key error does not explain the llm-step grant"
grep -qF 'reader=DEEP-DEPS/llm-call' key.err \
  || fail "missing-key error does not explain the title grant"
grep -qF "$CAOS_CLI secrets" key.err \
  || fail "missing-key error does not explain entropy setup"
if remote_tip "$queued_ref" >/dev/null; then
  fail "request-preparation failure partially admitted a conversation"
fi
[ ! -e stub/request-1.json ] || fail "missing-key failure reached the LLM"

echo "== invalid bases publish no conversation ==" >&2
mkdir -p .caos-secrets
printf '.caos-secrets/\n' >> .git/info/exclude
printf '%s\n' \
  'name=anthropic-api-key' \
  'value=test-key' \
  'entropy=0123456789abcdef0123456789abcdef' \
  'reader=DEEP-DEPS/llm-step' \
  > .caos-secrets/anthropic-api-key
mkdir -p bad/.caos
echo reserved > bad/.caos/marker
git add bad
git -c user.email=test@caos -c user.name=caos commit -qm "reserved base"
badbase=$(mkcommit "HEAD:bad" "bad base")
if "$CAOS_CLI" chat "$bad_conv" -m "hello" --base "$badbase" "${opts[@]}" 2>base.err; then
  fail "chat accepted a base with top-level .caos"
fi
grep -q "\.caos" base.err || fail "reserved-base error is unclear"
if remote_tip "refs/caos/v2/conversations/$bad_conv/head" >/dev/null; then
  fail "reserved-base failure created a conversation"
fi
[ ! -e stub/request-1.json ] || fail "reserved-base failure reached the LLM"

echo "== remote work survives loss of its submitting client ==" >&2
"$CAOS_CLI" talk --new -c "$conv" "fresh start" --base "$base" \
  "${opts[@]}" >talk.out 2>talk.err &
client_pid=$!
request_started=0
for _ in $(seq 1 150); do
  if [ -e stub/request-1.json ]; then
    request_started=1
    break
  fi
  if ! kill -0 "$client_pid" 2>/dev/null; then
    fail "client exited before the blocked request reached the LLM: $(cat talk.err)"
  fi
  sleep 0.2
done
[ "$request_started" -eq 1 ] || fail "request never reached the LLM"
kill "$client_pid" || fail "could not terminate the submitting client"
if wait "$client_pid"; then
  fail "terminated client unexpectedly exited successfully"
fi
client_pid=""

printf '{"content":[{"text":"%s","type":"text"}],"stop_reason":"end_turn"}' \
  "$TALK_TEXT" > stub/response-1.json

recovered=0
for _ in $(seq 1 150); do
  if tip=$(remote_tip "$ref"); then
    git fetch -q caos "$tip"
    history=$(git log --first-parent --format=%B "$tip")
    if [[ "$history" == *"$TALK_TEXT"* ]]; then
      recovered=1
      break
    fi
  fi
  sleep 0.2
done
[ "$recovered" -eq 1 ] || fail "worker did not finish after client disconnect"

capture_events "$tip" "$base" talk.events
[ "$EVENT_COUNT" -ge 4 ] || fail "terminal turn recorded only $EVENT_COUNT durable events"
jq -s -e 'any(.[]; .author == "user" and .content == "fresh start")' \
  talk.events >/dev/null || fail "user event is missing"
grep -q '"request":"[0-9a-f]\{40\}"' talk.events || fail "exact request was not recorded"
grep -qF "$TALK_TEXT" talk.events || fail "post-disconnect assistant event is missing"
[ "$(git show "$tip:notes/todo.txt")" = "hello notes" ] \
  || fail "completed conversation lost its base workspace"

admission=""
current=$tip
while [ "$current" != "$base" ]; do
  event=$(git show -s --format=%B "$current" | tr -d '\n')
  if jq -e '.status == "queued" and (.request | type == "string") and (.request_head | type == "string")' \
      <<<"$event" >/dev/null; then
    admission=$current
    break
  fi
  current=$(git rev-parse "$current^1")
done
[ -n "$admission" ] || fail "atomic request admission event is missing"
admission_event=$(git show -s --format=%B "$admission" | tr -d '\n')
request=$(jq -r '.request' <<<"$admission_event")
request_args=$(git ls-tree --name-only "$request")
grep -qx 'secret-hash' <<<"$request_args" \
  || fail "conversation request is not isolated by its model secret"
if grep -qx 'api-key' <<<"$request_args"; then
  fail "conversation request still contains a curried API key"
fi

title_ref="refs/caos/v2/conversations/$conv/title"
title=$(remote_tip "$title_ref") || fail "conversation has no title ref"
git fetch -q caos "$title"
[ "$(git cat-file blob "$title")" = "fresh start" ] || fail "conversation title is wrong"
git ls-remote --refs caos "refs/caos/v2/conversations/$conv/*" > conversation.refs
[ "$(wc -l < conversation.refs)" -eq 2 ] \
  || fail "conversation does not have exactly its head and title refs"

conversation_key="c-$(printf '%s' "$conv" | od -An -v -tx1 | tr -d '[:space:]')"
git ls-remote --refs caos \
  "refs/caos/v2/users/*/conversations/active/$conversation_key" > membership.refs
[ "$(wc -l < membership.refs)" -eq 1 ] \
  || fail "conversation does not have exactly one active creator membership"
legacy_refs=$(git ls-remote --refs caos "refs/caos/v2/conversations/$conv/from-*") \
  || fail "checking for legacy conversation refs"
[ -z "$legacy_refs" ] || fail "conversation created legacy refs"

echo "== a fresh client fetches and replays the conversation closure ==" >&2
server_url=$(git remote get-url caos)
mkdir fresh-reader
git -C fresh-reader init -q
git -C fresh-reader remote add caos "$server_url"
(
  cd fresh-reader
  "$CAOS_CLI" talk -c "$conv" --log > ../fresh-reader.log
)
grep -qF "tester: fresh start" fresh-reader.log \
  || fail "fresh client did not replay the selected conversation"
grep -qF "assistant: $TALK_TEXT" fresh-reader.log \
  || fail "fresh client could not fetch the selected conversation closure"

echo "chat-offline: ALL PASS" >&2
