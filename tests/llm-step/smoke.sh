#!/usr/bin/env bash
# Smoke test with a REAL key: one tiny `caos talk` turn against the live
# Anthropic API. Skipped (exit 0) unless ANTHROPIC_API_KEY is set, and NOT
# discovered by tests/run-all.sh (only cli.sh files are) — run it by hand:
#
#   ANTHROPIC_API_KEY=... tests/llm-step/smoke.sh
#
# It spends (a little) real money and needs the runner containers to have
# egress to api.anthropic.com. It is also the UX spec: one turn needs only a
# running stack, a git repo with a `caos` remote, and the key — the worker
# curries come from the published std (build-builtins.sh), nothing is built
# or committed locally.
set -euo pipefail

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "smoke: ANTHROPIC_API_KEY not set; skipping" >&2
  exit 0
fi

PROJECT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$PROJECT"

echo "smoke: building the CLI..." >&2
nix build .#caos-cli -o result-caos
caosbin=$PROJECT/result-caos/bin/caos # the `caos` name, as a person types it

export CAOS_DATA="${CAOS_DATA:-$PROJECT/.caos-data}"
echo "smoke: bringing the stack up (caosd up)..." >&2
nix run .#caosd -- up >&2

CLIENT=$PROJECT/.caos-dev/smoke-client
rm -rf "$CLIENT"
git init -q "$CLIENT"
git -C "$CLIENT" remote add caos "${CAOS_SERVER_URL:-http://localhost:9090}"
git -C "$CLIENT" config gc.auto 0
git -C "$CLIENT" config user.name smoke
git -C "$CLIENT" config user.email smoke@caos
trap 'rm -rf "$CLIENT"' EXIT

echo "smoke-test workspace" > "$CLIENT/README"
git -C "$CLIENT" add -A
git -C "$CLIENT" commit -qm "smoke workspace"

cd "$CLIENT"
echo "smoke: running one real turn..." >&2
# Cheapest model that supports adaptive thinking (the worker always sends
# thinking:{type:"adaptive"}; haiku-4-5 rejects it with a 400).
"$caosbin" talk --model claude-sonnet-5 \
  "Use the bash tool to run \`echo pong\`, then reply with just its output."
# A fresh repo has no conversations, so talk auto-named the first one talk-1.
git rev-parse -q --verify refs/caos/conversations/talk-1 >/dev/null \
  || { echo "smoke: FAIL — conversation ref missing" >&2; exit 1; }
echo "smoke: PASS" >&2
