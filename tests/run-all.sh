#!/usr/bin/env bash
# Run every integration test — each tests/<name>/ that has a cli.sh — via run.sh.
#
# The per-test ceremony is hoisted here and done ONCE: build the CLI, bring the
# stack up (cold the first time, a warm no-op after), then hand both to each
# run.sh via CAOS_CLI + CAOS_STACK_READY — sparing every test its two flake
# evals and cache-hit republish. run.sh stays self-contained when invoked bare
# (it does the ceremony itself when the env is absent). Per-test CAOS_SALT
# keeps runs independent, so no reset is needed between them. Leaves the stack
# running; `caosd down` stops it.
#
# Usage: tests/run-all.sh
# Exits non-zero if any test fails.
set -uo pipefail
cd "$(dirname "$0")/.."

mapfile -t dirs < <(for d in tests/*/; do [ -f "$d/cli.sh" ] && printf '%s\n' "${d%/}"; done)
[ "${#dirs[@]}" -gt 0 ] || { echo "no tests found under tests/" >&2; exit 2; }

echo "building caos client + bringing the stack up (once for the suite)..." >&2
nix build .#caos-cli -o result-caos || exit 1
export CAOS_CLI=$PWD/result-caos/bin/caos-cli
export CAOS_DATA="${CAOS_DATA:-$PWD/.caos-data}"
nix run .#caosd -- up >&2 || exit 1
export CAOS_STACK_READY=1

pass=(); fail=()
for t in "${dirs[@]}"; do
  echo "=== $t ===" >&2
  if tests/run.sh "$t"; then pass+=("$t"); else fail+=("$t"); fi
done

echo >&2
echo "==== ${#pass[@]}/${#dirs[@]} passed ====" >&2
for t in "${pass[@]}"; do echo "  PASS $t" >&2; done
for t in "${fail[@]}"; do echo "  FAIL $t" >&2; done
[ "${#fail[@]}" -eq 0 ]
