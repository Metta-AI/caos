#!/usr/bin/env bash
# Smoke test with a REAL key: one tiny `caos-cli chat` turn against the live
# Anthropic API. Skipped (exit 0) unless ANTHROPIC_API_KEY is set, and NOT
# discovered by tests/run-all.sh (only cli.sh files are) — run it by hand:
#
#   ANTHROPIC_API_KEY=... tests/llm-step/smoke.sh
#
# It spends (a little) real money and needs the runner containers to have
# egress to api.anthropic.com. Everything else matches the stub tests: the
# stack comes up via caosd, the worker binaries are nix-built and committed
# into a throwaway client repo, and the turn runs through the real CLI verb.
set -euo pipefail

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "smoke: ANTHROPIC_API_KEY not set; skipping" >&2
  exit 0
fi

PROJECT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$PROJECT"

echo "smoke: building the CLI and worker binaries..." >&2
nix build .#caos-cli -o result-caos
caosbin=$PROJECT/result-caos/bin/caos-cli
nix build .#worker-bash-tool -o smoke-bash-out
nix build .#worker-llm-step -o smoke-llm-out

export CAOS_SERVER_URL=${CAOS_SERVER_URL:-http://localhost:9090}
export CAOS_DATA="${CAOS_DATA:-$PROJECT/.caos-data}"
export CAOS_SALT="${CAOS_SALT:-$(date +%s%N)-$$}"
echo "smoke: bringing the stack up (caosd up)..." >&2
nix run .#caosd -- up >&2

CLIENT=$PROJECT/.caos-dev/smoke-client
rm -rf "$CLIENT"
git init -q "$CLIENT"
git -C "$CLIENT" remote add caos "$CAOS_SERVER_URL"
git -C "$CLIENT" config gc.auto 0
git -C "$CLIENT" config user.name smoke
git -C "$CLIENT" config user.email smoke@caos
trap 'rm -rf "$CLIENT" smoke-bash-out smoke-llm-out' EXIT

cp -L smoke-bash-out/bin/worker-bash-tool "$CLIENT/bash-tool-bin"
cp -L smoke-llm-out/bin/worker-llm-step "$CLIENT/llm-step-bin"
echo "smoke-test workspace" > "$CLIENT/README"
git -C "$CLIENT" add -A
git -C "$CLIENT" commit -qm "smoke workspace"

cd "$CLIENT"
export CAOS_LLM_STEP_BIN=llm-step-bin CAOS_BASH_TOOL_BIN=bash-tool-bin
conv="smoke-$(date +%s)"
echo "smoke: running one real turn (conversation $conv)..." >&2
# Cheapest model that supports adaptive thinking (the worker always sends
# thinking:{type:"adaptive"}; haiku-4-5 rejects it with a 400).
"$caosbin" chat "$conv" --model claude-sonnet-5 \
  -m "Use the bash tool to run \`echo pong\`, then reply with just its output."
git rev-parse -q --verify "refs/caos/conversations/$conv" >/dev/null \
  || { echo "smoke: FAIL — conversation ref missing" >&2; exit 1; }
echo "smoke: PASS" >&2
