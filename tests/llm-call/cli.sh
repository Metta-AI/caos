#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, inside a test stack (tests/lib/run-test.sh).
#
# Exercises the generic stateless LLM call worker against the scripted API
# stub: caller-owned prompt/messages/config in, plain text blob out, with no
# agent tools, thinking mode, commits, or conversation refs.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

# The stub, from its std entry (std/llm-stub): a cargo `--cmd=build` result, so
# the executable is at bin/<name>. Copied out because materialized CAS content
# is read-only and owner-only — exec straight from /cas is "Permission denied".
"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/llm-stub-entry || fail "resolving std/llm-stub"
stub_bin=/tmp/llm-stub-bin
install -m 755 /tmp/llm-stub-entry/bin/llm-stub "$stub_bin"

mkdir stub
printf '%s' '{"content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"Improve sidebar titles"}],"stop_reason":"end_turn"}' \
  > stub/response-1.json

stub_pid=""
for _ in 1 2 3 4 5; do
  port=$((20000 + RANDOM % 20000))
  "$stub_bin" "0.0.0.0:$port" "$PWD/stub" 2>stub/log &
  stub_pid=$!
  # Wait for the LISTENER, not for a fixed interval: a flat `sleep 0.5` here
  # was half a second of a test whose whole body is a few seconds, and the stub
  # binds in a few ms. Probing the port also tells the two failures apart — a
  # dead process (retry on another port) against one still coming up.
  ready=0
  for _ in {1..400}; do
    if ! kill -0 "$stub_pid" 2>/dev/null; then break; fi
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then ready=1; break; fi
    sleep 0.005
  done
  if [ "$ready" = 1 ]; then break; fi
  kill "$stub_pid" 2>/dev/null || true
  stub_pid=""
done
[ -n "$stub_pid" ] || fail "could not start llm-stub: $(cat stub/log)"
trap 'kill "$stub_pid" 2>/dev/null || true' EXIT

stub_host=${CAOS_STUB_HOST:-host.containers.internal}
mkdir -p .caos-secrets
printf '.caos-secrets/\n' >> .git/info/exclude
printf '%s\n' \
  'name=anthropic-api-key' \
  'value=test-key' \
  'entropy=0123456789abcdef0123456789abcdef' \
  'reader=DEEP-DEPS/llm-call' \
  > .caos-secrets/anthropic-api-key
call=$(
  "$CAOS_CLI" curry --base:@=DEEP-DEPS/llm-call \
    --base-url="http://$stub_host:$port"
)
messages='[{"role":"user","content":"Name the sidebar task"}]'
result=$(
  "$CAOS_CLI" run --base:hash="$call" \
    --system='Return only a concise title.' \
    --messages="$messages" \
    --model=test-model \
    --max-tokens=64
)

[ "$result" = "Improve sidebar titles" ] || fail "unexpected response: $result"
grep -qF '"model":"test-model"' stub/request-1.json || fail "model not sent"
grep -qF '"max_tokens":64' stub/request-1.json || fail "max_tokens not sent"
grep -qF '"system":"Return only a concise title."' stub/request-1.json \
  || fail "system prompt not sent"
grep -qF '"messages":[{"content":"Name the sidebar task","role":"user"}]' \
  stub/request-1.json || fail "messages not sent"
if grep -q '"tools"' stub/request-1.json; then
  fail "stateless call registered tools"
fi
if grep -q '"thinking"' stub/request-1.json; then
  fail "stateless call enabled thinking"
fi
[ ! -f stub/request-2.json ] || fail "worker made more than one model call"

echo "llm-call: ALL PASS" >&2
