#!/usr/bin/env bash
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }

# No GitHub account or external mutation: version/help exercise the real image,
# extension, secret grant, invocation record, and agent callback.
mkdir -p .caos-secrets
printf '.caos-secrets/\n' >> .git/info/exclude
printf '%s\n' 'value=github-fixture-token' 'entropy=0123456789abcdef0123456789abcdef' \
  'reader=DEEP-DEPS/github' 'reader=DEEP-DEPS/llm-step' > .caos-secrets/github-token
printf '%s\n' 'value=model-fixture-token' 'entropy=abcdef0123456789abcdef0123456789' \
  'reader=DEEP-DEPS/llm-step' > .caos-secrets/anthropic-api-key
id=$(printf '%s' "$(date +%s%N)-$$-$RANDOM" | sha256sum)
id=${id%% *}
"$CAOS_CLI" run version --base:@=DEEP-DEPS/github --repository=owner/repo \
  --args='["--version"]' --invocation="$id"
jq -e '.status == "complete" and .exit == 0 and (.stdout | contains("gh version"))' version >/dev/null
"$CAOS_CLI" run repeated --base:@=DEEP-DEPS/github --repository=owner/repo \
  --args='["--version"]' --invocation="$id"
cmp version repeated
if "$CAOS_CLI" run changed --base:@=DEEP-DEPS/github --repository=owner/repo \
  --args='["--help"]' --invocation="$id" 2>changed.err; then
  fail "an invocation accepted changed arguments"
fi
grep -q "different request" changed.err || fail "unclear invocation error"

"$CAOS_CLI" get DEEP-DEPS/llm-stub /tmp/stub-entry
install -m 755 /tmp/stub-entry/bin/llm-stub /tmp/github-llm-stub
mkdir stub
printf '%s\n' '{"content":[{"type":"tool_use","id":"gh","name":"github","input":{"repository":"owner/repo","args":["stack","link","--help"]}}],"stop_reason":"tool_use"}' > stub/response-1.json
printf '%s\n' '{"content":[{"type":"text","text":"checked"}],"stop_reason":"end_turn"}' > stub/response-2.json
port=$((20000 + RANDOM % 20000))
/tmp/github-llm-stub "0.0.0.0:$port" "$PWD/stub" 2>stub/log &
stub_pid=$!
trap 'kill "$stub_pid" 2>/dev/null || true' EXIT
ready=0
for _ in $(seq 1 400); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then ready=1; break; fi
  sleep 0.01
done
[ "$ready" = 1 ] || fail "stub did not start"
"$CAOS_CLI" chat "github-$id" -m "Check stack link help." --username tester --model test-model \
  --base-url "http://${CAOS_STUB_HOST:-host.containers.internal}:$port" \
  --llm-step:@=DEEP-DEPS/llm-step
jq -e '.tools | any(.name == "github")' stub/request-1.json >/dev/null
jq -e '.tools[] | select(.name == "publish_source") | .input_schema |
  (.properties.source_tree.type == "string") and
  (.required | index("source_tree") != null)' stub/request-1.json >/dev/null
jq -e '[.messages[].content | select(type == "array") | .[] |
  select(.type == "tool_result" and .tool_use_id == "gh")] |
  length == 1 and .[0].is_error != true and
  (.[0].content | map(.text) | join("") | contains("gh stack link"))' stub/request-2.json >/dev/null
echo "github: ALL PASS"
