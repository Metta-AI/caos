#!/usr/bin/env bash
# Checked-in repository instructions (.caos/agent.json — SPEC "Repository
# agent instructions"). A base tree carrying only .caos/agent.json must be
# accepted, and the harness must fold the file's `instructions` into the
# system prompt it sends the model; any other .caos entry beside it must
# still be refused before admission.
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
  printf '%s\n' "${lines%%[[:space:]]*}"
}

echo "== stage fixture and scripted LLM ==" >&2
"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry \
  || fail "resolving std/llm-stub"
stub_bin=/tmp/llm-stub-bin
install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"

INSTRUCTIONS="Repo rule: mention the word shibboleth when you finish."
mkdir -p ws/.caos
echo "hello workspace" > ws/file.txt
printf '{"instructions":"%s","future-field":true}\n' "$INSTRUCTIONS" > ws/.caos/agent.json
mkdir -p ws-bad/.caos
cp ws/.caos/agent.json ws-bad/.caos/agent.json
echo reserved > ws-bad/.caos/marker
echo "hello workspace" > ws-bad/file.txt
commit "agent-instructions fixture"
git config user.name tester
git config user.email tester@example.com
base=$(mkcommit "HEAD:ws" "instructed base")
badbase=$(mkcommit "HEAD:ws-bad" "over-reserved base")

mkdir -p .caos-secrets
printf '.caos-secrets/\n' >> .git/info/exclude
printf '%s\n' \
  'name=anthropic-api-key' \
  'value=test-key' \
  'entropy=0123456789abcdef0123456789abcdef' \
  'reader=DEEP-DEPS/llm-step' \
  > .caos-secrets/anthropic-api-key

mkdir stub
stub_pid=""
cleanup() {
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
conv="${test_id}-instructed"
bad_conv="${test_id}-over-reserved"
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
opts=(--model test-model --base-url "http://$stub_host:$port")

echo "== a .caos entry beside agent.json is still refused ==" >&2
if "$CAOS_CLI" chat "$bad_conv" -m "hello" --base "$badbase" "${opts[@]}" 2>base.err; then
  fail "chat accepted a base with reserved .caos state beside agent.json"
fi
grep -q "\.caos" base.err || fail "reserved-base error is unclear: $(cat base.err)"
grep -q "marker" base.err || fail "reserved-base error does not name the entry"
if remote_tip "refs/caos/v2/conversations/$bad_conv/head" >/dev/null; then
  fail "reserved-base failure created a conversation"
fi
[ ! -e stub/request-1.json ] || fail "reserved-base failure reached the LLM"

echo "== agent.json instructions reach the system prompt ==" >&2
printf '{"content":[{"text":"noted, shibboleth","type":"text"}],"stop_reason":"end_turn"}' \
  > stub/response-1.json
"$CAOS_CLI" chat "$conv" -m "read the repo rules" --base "$base" "${opts[@]}" \
  >chat.out 2>chat.err || fail "instructed turn failed: $(cat chat.err)"
[ -e stub/request-1.json ] || fail "the turn never reached the LLM"
jq -e --arg wanted "$INSTRUCTIONS" '.system | contains($wanted)' \
  stub/request-1.json >/dev/null \
  || fail "system prompt is missing the repository instructions: $(jq -r '.system' stub/request-1.json)"
jq -e '.system | contains("Repository instructions (.caos/agent.json)")' \
  stub/request-1.json >/dev/null \
  || fail "system prompt does not attribute the repository instructions"
jq -e '.system | startswith("You are a coding agent")' stub/request-1.json >/dev/null \
  || fail "repository instructions replaced the curried system prompt instead of extending it"

echo "== the published head keeps the checked-in agent.json ==" >&2
tip=$(remote_tip "refs/caos/v2/conversations/$conv/head") \
  || fail "instructed conversation has no head"
git fetch -q caos "$tip"
[ "$(git show "$tip:.caos/agent.json" | jq -r '.instructions')" = "$INSTRUCTIONS" ] \
  || fail "the conversation head lost .caos/agent.json"
[ "$(git show "$tip:file.txt")" = "hello workspace" ] \
  || fail "the conversation head lost its base workspace"

echo "agent-instructions: ALL PASS" >&2
