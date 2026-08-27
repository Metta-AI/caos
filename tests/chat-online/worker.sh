#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (dev/cli-test stages the repo, then runs this).
#
# One REAL turn against the live Anthropic API — the one check the stub suites
# structurally can't make: only the live API rejects a bad model choice (e.g.
# the adaptive-thinking-on-haiku 400 this once caught). Discovered and run like
# every other test, but it needs a real key, spends (a little) real money, and
# needs runner egress to api.anthropic.com — so without one it skips (exit 0;
# the report shows it as a PASS, with the skip as its output).
#
# THE KEY ARRIVES AT /secret, not in the environment. A secret store does not
# cross a stack: the caller's `.caos-secrets` is resolved by ITS client and
# granted within ITS call stack, and this turn runs two stacks down. So
# `caos-tools/test` — a declared reader on the host — re-registers the value as
# a store for the dev stack naming `tests/chat-online` as reader, and the server
# grants it here because this job's ArgTree is a superset of that reader's.
#
# It doubles as the UX spec: everything above the `talk` line is the generic
# test harness — a real turn itself is one command.
set -euo pipefail

echo "chat-online: fails even with a valid anthropic-api-key. Skipping"
exit 0

if [ ! -e /secret/anthropic-api-key ]; then
  echo "chat-online: no anthropic-api-key granted — SKIPPED (no real-API turn run)" >&2
  exit 0
fi
key=$(cat /secret/anthropic-api-key)

# The store this test's OWN turn needs: `talk` spawns llm-step, which reads the
# key from /secret in its turn. Git-excluded so it cannot be ingested.
mkdir -p .caos-secrets
printf '.caos-secrets/\n' >> .git/info/exclude
printf '%s\n' \
  'name=anthropic-api-key' \
  "value=$key" \
  'entropy=0123456789abcdef0123456789abcdef' \
  'reader=DEEP-DEPS/llm-step' \
  > .caos-secrets/anthropic-api-key

# The human commit carries the client's git identity; pin one so the test
# doesn't depend on host-global config.
git config user.name chat-online-test
git config user.email chat-online-test@caos

# Cheapest model that supports adaptive thinking (the worker always sends
# thinking:{type:"adaptive"}; haiku-4-5 rejects it with a 400).
test_run_id="$(date +%s%N)-$$-$RANDOM"
conv="${test_run_id}-talk-online"
"$CAOS_CLI" talk --new -c "$conv" --model claude-sonnet-5 \
  "Use the bash tool to run \`echo pong\`, then reply with just its output."

git rev-parse -q --verify "refs/caos/v2/conversations/$conv/head" >/dev/null \
  || { echo "chat-online: FAIL — conversation ref missing" >&2; exit 1; }
echo "chat-online: one real turn PASSED (conversation $conv)" >&2
