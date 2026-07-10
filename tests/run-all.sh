#!/usr/bin/env bash
# Run every integration test — each tests/<name>/ that has a cli.sh — via run.sh.
#
# run.sh is self-contained (it does `caosd up` itself), so this just discovers
# the test dirs, runs each, and summarizes. The stack is brought up once (cold)
# by the first test and stays warm for the rest, so the repeated `caosd up`s are
# near-instant. Per-test CAOS_SALT keeps runs independent, so no reset is needed
# between them. Leaves the stack running; `caosd down` stops it.
#
# Usage: tests/run-all.sh
# Exits non-zero if any test fails.
set -uo pipefail
cd "$(dirname "$0")/.."

mapfile -t dirs < <(for d in tests/*/; do [ -f "$d/cli.sh" ] && printf '%s\n' "${d%/}"; done)
[ "${#dirs[@]}" -gt 0 ] || { echo "no tests found under tests/" >&2; exit 2; }

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
