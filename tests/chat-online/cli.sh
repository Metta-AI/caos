#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI)
# ----------------------------------------------------------
# The subject of this test IS the tested client's real-turn path: `caos-cli
# talk` compiled from the tree under test, driving a live turn end to end. That
# path is not incidental scaffolding here — it is the whole point. A single
# `talk` invocation exercises, in one shot and only through the tested binary:
#   - model selection (the `--model` a real user passes),
#   - the worker's thinking config (it always sends thinking:{type:"adaptive"}),
#   - the tool-use loop (the model is asked to actually run the bash tool), and
#   - the conversation-ref write the turn must leave behind on success.
# None of these can be pinned down without running the tested client against a
# REAL Anthropic API, because that is the one endpoint the stub suites replace.
# The stubs can prove our request/response plumbing; they structurally CANNOT
# prove that the real API accepts the exact request our build emits. Only the
# live API rejects a bad combination — e.g. the adaptive-thinking-on-haiku 400
# this once caught — so only a turn driven by the tested `talk` can catch it.
# Swapping in any other binary (the host's /bin/caos, a helper, a curl) would
# test a DIFFERENT build's or a hand-rolled request, defeating the purpose:
# what we are pinning is that THIS client, as built, completes a real turn.
#
# So this is a genuine $CAOS_CLI test, and deliberately the only one that spends
# real money and needs runner egress to api.anthropic.com. It needs a real key;
# without ANTHROPIC_API_KEY it skips (exit 0; run-all shows it as a PASS, with
# the skip on stderr).
#
# It doubles as the UX spec: everything above the `talk` line is the generic
# test harness — a real turn itself is one command.
set -euo pipefail

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "chat-online: ANTHROPIC_API_KEY not set — SKIPPED (no real-API turn run)" >&2
  exit 0
fi

mkdir -p .caos-secrets
printf '.caos-secrets/\n' >> .git/info/exclude
printf '%s\n' \
  'name=anthropic-api-key' \
  "value=$ANTHROPIC_API_KEY" \
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
