#!/usr/bin/env bash
#@doc Print one test's COMPLETE record from a `test` run: the test's whole
#@doc output — the report only carries its last few lines — or, with `log`,
#@doc one of the inner stack's logs. Takes the hash the test report prints
#@doc beside each test's name; that hash is stable, so the record of a run
#@doc stays readable long after the run.
#@arg hash The hash the `test` report prints beside a test's name.
#@arg [log] Print this inner-stack log instead of the test's output: server, runnerd, redis or serve.
#
# The counterpart to caos-tools/test.sh's report. A test's record —
# {verdict, seconds, output, server.log, runnerd.log, ...} — already rides in
# the suite result (tests/lib/run-test.sh writes it), so nothing here re-runs
# anything: this is a READ, addressed by hash, of a tree the suite already
# published. That is why the report can afford to be short.
#
# The result is a BLOB, not a tree with a `report`: both readers print a blob
# verbatim (crates/caos/src/lib.rs's report_conventions, and the harness's
# tree_tool_result_block), and a `report` would be scanned for a FAILED banner
# — which a failing test's log says all the time, so the reader would call
# this tool's own success a failure.
#
# EVERY OUTCOME IS THE VALUE, including "no such record". A bad hash must come
# back as text the caller can read and correct, not as a job error: this tool
# is called precisely when something already went wrong, and a worker error
# there would take the agent's turn down with it.
set -euo pipefail

out=/tmp/out

# A bad hash is answered, not raised. `caos get-hash` on a nonexistent object
# fails the job, so the shape is checked first and the fetch's own failure is
# caught rather than propagated.
caos get /cas/args/hash
hash=$(tr -d '[:space:]' < /cas/args/hash)
if ! printf '%s' "$hash" | grep -qE '^[0-9a-f]{40}$'; then
  printf 'not a hash: %s\n\nPass the 40-character hash the `test` report prints beside a test.\n' \
    "$hash" > "$out"
  caos put "$out" /cas/out
  exit 0
fi

want=output
if [ -e /cas/args/log ]; then
  caos get /cas/args/log
  want="$(tr -d '[:space:]' < /cas/args/log).log"
fi

if ! caos get-hash "$hash" /cas/rec 2>/tmp/fetch.err; then
  { printf 'no object %s on this server:\n\n' "$hash"; cat /tmp/fetch.err; } > "$out"
  caos put "$out" /cas/out
  exit 0
fi

# `get-hash` materialized the record itself, so its entries are placeholders:
# fetch ONLY the one being printed. A record carries the inner stack's logs
# alongside the output, and pulling the lot to show one of them would move
# megabytes for nothing.
if [ ! -d /cas/rec ]; then
  printf '%s is not a test record (it is a blob, not a tree)\n' "$hash" > "$out"
  caos put "$out" /cas/out
  exit 0
fi
if [ ! -e "/cas/rec/$want" ]; then
  { printf '%s has no %s. It holds: ' "$hash" "$want"
    ls -A /cas/rec | tr '\n' ' '
    printf '\n\nA test record comes from the `results/<test>` entries of a `test` run.\n'
  } > "$out"
  caos put "$out" /cas/out
  exit 0
fi

{
  # The verdict and the elapsed line first, so the header says what this is
  # even when the body is a wall of cargo output.
  for meta in verdict seconds; do
    if [ -e "/cas/rec/$meta" ]; then
      caos get "/cas/rec/$meta"
      printf '%s: %s\n' "$meta" "$(cat "/cas/rec/$meta")"
    fi
  done
  printf -- '---- %s ----\n' "$want"
  caos get "/cas/rec/$want"
  cat "/cas/rec/$want"
} > "$out"
caos put "$out" /cas/out
