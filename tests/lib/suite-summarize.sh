#!/bin/bash
# Suite `then`: every test job's result tree arrives under --children (by
# test name): {verdict, output, server.log, runnerd.log, ...} — the complete
# record. Assemble the report — one PASS/FAIL line per test, ending in an
# OK/FAILED banner — and carry the children through verbatim as `results`
# (a symlink put: recorded-hash reuse, no bytes move). The suite job itself
# always SUCCEEDS with a report; the caller decides what a FAILED banner
# means. Failures are values here so one broken test never hides the others.
set -euo pipefail

caos get /cas/args/children
mkdir -p /tmp/rep
passn=0 failn=0
{
  for c in /cas/args/children/*; do
    t=$(basename "$c")
    caos get "/cas/args/children/$t"
    caos get "/cas/args/children/$t/verdict"
    if grep -q "^RUN-TEST: PASS" "$c/verdict"; then
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
ln -s /cas/args/children /tmp/rep/results
caos put /tmp/rep /cas/out
