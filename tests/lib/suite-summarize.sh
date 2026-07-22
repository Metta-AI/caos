#!/bin/bash
# Suite `then`: every test job's verdict blob arrives under --children (by
# test name). Assemble the report — one PASS/FAIL line per test, the report
# ending in an OK/FAILED banner — plus the raw verdicts (a failing one
# carries its cli.sh output tail). The suite job itself always SUCCEEDS with
# a report; the caller decides what a FAILED banner means. Failures are
# values here so one broken test never hides the others' results.
set -euo pipefail

caos get /cas/args/children
mkdir -p /tmp/rep/verdicts
passn=0 failn=0
{
  for c in /cas/args/children/*; do
    t=$(basename "$c")
    caos get "/cas/args/children/$t"
    cp "$c" "/tmp/rep/verdicts/$t"
    if grep -q "^RUN-TEST: PASS" "$c"; then
      echo "PASS tests/$t"; passn=$((passn + 1))
    else
      echo "FAIL tests/$t"; failn=$((failn + 1))
    fi
  done
  echo
  if [ "$failn" -eq 0 ]; then
    echo "SUITE OK: $passn/$((passn + failn)) passed"
  else
    echo "SUITE FAILED: $passn/$((passn + failn)) passed"
  fi
} > /tmp/rep/report
caos put /tmp/rep /cas/out
