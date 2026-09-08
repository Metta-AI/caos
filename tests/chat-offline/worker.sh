#!/usr/bin/env bash
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
mkcommit() {
  local tree=$1 message=$2 parent=${3:-}
  local parents=()
  if [ -n "$parent" ]; then parents=(-p "$parent"); fi
  git -c user.email=test@caos -c user.name=caos \
    commit-tree "$tree" "${parents[@]}" -m "$message"
}
remote_tip() {
  local lines
  lines=$(git ls-remote --refs caos "$1") || return 1
  [ -n "$lines" ] || return 1
  [ "${lines#*$'\n'}" = "$lines" ] || fail "remote advertised $1 more than once"
  printf '%s\n' "${lines%%[[:space:]]*}"
}

echo "== stage fixture and scripted LLM ==" >&2
"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry \
  || fail "resolving std/llm-stub"
"$CAOS_CLI" get DEEP-DEPS/llm-test-tool /tmp/llm-test-tool-entry \
  || fail "resolving std/llm-test-tool"
stub_bin=/tmp/llm-stub-bin
TOOL=/tmp/llm-test-tool
install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"
install -m 755 /tmp/llm-test-tool-entry/bin/llm-test-tool "$TOOL"

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
ref=$($TOOL ref --id "$conv")
ref=${ref#ref }
queued_conv="${test_id}-queued-chat"
queued_ref=$($TOOL ref --id "$queued_conv")
queued_ref=${queued_ref#ref }
bad_conv="${test_id}-bad-chat"
bad_ref=$($TOOL ref --id "$bad_conv")
bad_ref=${bad_ref#ref }
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

echo "== a conversation-shaped base is refused ==" >&2
mkdir -p .caos-secrets
printf '.caos-secrets/\n' >> .git/info/exclude
printf '%s\n' \
  'name=anthropic-api-key' \
  'value=test-key' \
  'entropy=0123456789abcdef0123456789abcdef' \
  'reader=DEEP-DEPS/llm-step' \
  > .caos-secrets/anthropic-api-key
fake_output=$($TOOL root --repo "$PWD" --id "${test_id}-fake-base" --title fake)
fake_base=${fake_output#head }
if "$CAOS_CLI" chat "$bad_conv" -m "hello" --base "$fake_base" "${opts[@]}" 2>base.err; then
  fail "chat accepted a base descending from G3"
fi
grep -Eq 'G3|conversation' base.err || fail "conversation-base error is unclear"
if remote_tip "$bad_ref" >/dev/null; then
  fail "conversation-base failure created a conversation"
fi
[ ! -e stub/request-1.json ] || fail "conversation-base failure reached the LLM"

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
tip=""
seen=""
for _ in $(seq 1 150); do
  if remote=$(remote_tip "$ref") && [ "$remote" != "$seen" ]; then
    seen=$remote
    fetch_output=$($TOOL fetch --repo "$PWD" --ref "$ref") \
      || fail "fetching the changed conversation ref"
    tip=${fetch_output#head }
    transcript=$($TOOL transcript --repo "$PWD" --head "$tip")
    if grep -qF "$TALK_TEXT" <<<"$transcript"; then
      recovered=1
      break
    fi
  fi
  sleep 0.25
done
[ "$recovered" -eq 1 ] || fail "worker did not finish after client disconnect"

echo "== canonical refs, title, spine, workspace, and request isolation ==" >&2
conversation_prefix=${ref%/head}
git ls-remote --refs caos "$conversation_prefix/*" > conversation.refs
[ "$(wc -l < conversation.refs)" -eq 1 ] \
  || fail "conversation does not have exactly one canonical ref"
conversation_key=$(printf '%s' "$conv" | od -An -v -tx1 | tr -d '[:space:]')
git ls-remote --refs caos \
  "refs/caos/v3/users/*/conversations/active/$conversation_key" > membership.refs
[ "$(wc -l < membership.refs)" -eq 1 ] \
  || fail "conversation does not have exactly one active creator membership"
[ "$($TOOL read --repo "$PWD" --head "$tip" --path .caos/title)" = "fresh start" ] \
  || fail "conversation title is wrong"

$TOOL parents --repo "$PWD" --head "$tip" --validate > talk.parents \
  || fail "invalid conversation spine"
last_kind=""
while read -r _spine_commit spine_kind; do
  last_kind=$spine_kind
done < talk.parents
[ "$last_kind" = conversation.root ] || fail "conversation spine did not end at its root"
[ "$(git rev-list --first-parent "$tip" | tail -1)" = a2519b3360c5b1ded9a8cb7e5869d32901eae743 ] \
  || fail "conversation spine did not end at G3"

workspace_output=$($TOOL workspace --repo "$PWD" --head "$tip" --name main)
workspace=${workspace_output%%$'\n'*}
workspace=${workspace#commit }
git -c fetch.negotiationAlgorithm=noop fetch -q caos "$workspace" \
  || fail "fetching main workspace"
[ "$(git show "$workspace:notes/todo.txt")" = "hello notes" ] \
  || fail "completed conversation lost its base workspace"

request_path=$(git ls-tree -r --name-only "$tip" .caos/requests | grep '\.json$' | tail -1)
[ -n "$request_path" ] || fail "admission request record is missing"
admission=$($TOOL read --repo "$PWD" --head "$tip" --path "$request_path")
request=$(jq -r .id <<<"$admission")
request_args=$(git ls-tree --name-only "$request")
grep -qx 'secret-hash' <<<"$request_args" \
  || fail "conversation request is not isolated by its model secret"
if grep -qx 'api-key' <<<"$request_args"; then
  fail "conversation request still contains a curried API key"
fi

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
