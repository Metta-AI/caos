#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a testenv worker (tests/lib/run-nested.sh).
#
# Proves the generic worker-tool boundary through the real chat client:
# selection from a tracked tool-set tree, exact schema advertisement, dispatch
# to an independently curried worker image, standardized result handling, and
# omission when the next conversation has no --tools selection.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
mkcommit() {
  local tree=$1 msg=$2 parent=${3:-}
  git -c user.email=test@caos -c user.name=caos \
    commit-tree "$tree" ${parent:+-p "$parent"} -m "$msg"
}

cp "$CAOS_BIN_DIR/worker-bash-tool" bash-tool-bin
cp "$CAOS_BIN_DIR/worker-llm-step" llm-step-bin
cp "$CAOS_BIN_DIR/worker-rgrep" rgrep-bin
stub_bin=$CAOS_BIN_DIR/llm-stub
cp test/fixture-worker.sh fixture-worker.sh
chmod +x fixture-worker.sh

mkdir ws
printf 'workspace-marker' > ws/marker.txt
commit "stage fixture inputs"
base=$(mkcommit "HEAD:ws" "base")

fixture_image=$("$CAOS_CLI" curry /cas/std/bash -- --worker1:@=fixture-worker.sh)
mkdir -p toolset/fixture
printf '%s\n' "$fixture_image" > toolset/fixture/image
printf '%s\n' '{
  "name": "fixture",
  "description": "Read a message and the workspace marker using a generic worker.",
  "input_schema": {
    "type": "object",
    "properties": {"message": {"type": "string"}},
    "required": ["message"],
    "additionalProperties": false
  }
}' > toolset/fixture/tool.json
commit "generic worker tool set"

R1='[{"id":"toolu_fixture","input":{"message":"hello"},"name":"fixture","type":"tool_use"}]'
mkdir stub
printf '{"content":%s,"stop_reason":"tool_use"}' "$R1" > stub/response-1.json
printf '{"content":[{"text":"fixture complete","type":"text"}],"stop_reason":"end_turn"}' \
  > stub/response-2.json
printf '{"content":[{"text":"no tools complete","type":"text"}],"stop_reason":"end_turn"}' \
  > stub/response-3.json

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

export ANTHROPIC_API_KEY=test-key
export CAOS_LLM_STEP_BIN=llm-step-bin
export CAOS_BASH_TOOL_BIN=bash-tool-bin
export CAOS_RGREP_BIN=rgrep-bin
stub_host=${CAOS_STUB_HOST:-host.containers.internal}
opts=(--model test-model --base-url "http://$stub_host:$port")

conv="worker-tools-$(printf '%s' "${CAOS_SALT:-dev}" | tr -cd '0-9a-zA-Z')"
"$CAOS_CLI" chat "$conv" -m "use the fixture" --base "$base" \
  --tools toolset "${opts[@]}" > turn.out

grep -qF '"name":"fixture"' stub/request-1.json || fail "fixture tool not advertised"
grep -qF 'Read a message and the workspace marker using a generic worker.' \
  stub/request-1.json || fail "fixture description not advertised"
grep -qF '"additionalProperties":false' stub/request-1.json \
  || fail "fixture input schema not advertised"
grep -qF '"required":["message"]' stub/request-1.json \
  || fail "fixture required field not advertised"
grep -qF 'fixture saw hello and workspace-marker' stub/request-2.json \
  || fail "generic worker result missing from the next request"
grep -qF 'fixture complete' turn.out || fail "turn did not complete"

without="worker-tools-none-$(printf '%s' "${CAOS_SALT:-dev}" | tr -cd '0-9a-zA-Z')"
"$CAOS_CLI" chat "$without" -m "answer without tools" --base "$base" \
  "${opts[@]}" > without.out
grep -qF '"name":"fixture"' stub/request-3.json \
  && fail "fixture tool was advertised without --tools"
grep -qF 'no tools complete' without.out || fail "tool-less turn did not complete"

echo "worker-tools: ALL PASS" >&2
