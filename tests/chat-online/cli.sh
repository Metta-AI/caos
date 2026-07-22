#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a testenv worker — the suite's per-test job
# (tests/lib/run-nested.sh).
#
# One REAL turn against the live Anthropic API — the one check the stub suites
# structurally can't make: only the live API rejects a bad model choice (e.g.
# the adaptive-thinking-on-haiku 400 this once caught). Discovered and run like
# every other test, but it needs a real key, spends (a little) real money, and
# needs runner egress to api.anthropic.com — so without ANTHROPIC_API_KEY it
# skips (exit 0; run-all shows it as a PASS, with the skip on stderr).
#
# It doubles as the UX spec: everything above the `talk` line is the generic
# test harness — a real turn itself is one command.
set -euo pipefail

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "chat-online: ANTHROPIC_API_KEY not set — SKIPPED (no real-API turn run)" >&2
  exit 0
fi

# The human commit carries the client's git identity; pin one so the test
# doesn't depend on host-global config.
git config user.name chat-online-test
git config user.email chat-online-test@caos

# Cheapest model that supports adaptive thinking (the worker always sends
# thinking:{type:"adaptive"}; haiku-4-5 rejects it with a 400).
"$CAOS_CLI" talk --model claude-sonnet-5 \
  "Use the bash tool to run \`echo pong\`, then reply with just its output."

# A fresh repo has no conversations, so talk auto-named the first one talk-1.
git rev-parse -q --verify refs/caos/conversations/talk-1 >/dev/null \
  || { echo "chat-online: FAIL — conversation ref missing" >&2; exit 1; }
echo "chat-online: one real turn PASSED (conversation talk-1)" >&2
