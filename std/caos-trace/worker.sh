#!/usr/bin/env bash
# The `caos-trace` tool's worker. Its DOCS live in the sibling `.caos-expr`
# here-string, not here.
#
# A READ, addressed by hash: `caos status --all` asks the server for the run's
# trace (SPEC, "Tracing") and every out-trace object the nodes name is fetched
# with `caos get-hash`. Nothing re-runs.
#
# EVERY OUTCOME IS THE VALUE, as in caos-test-result: this tool is called when
# something already went wrong or ran slowly, and a bad hash or an empty trace
# must come back as text the caller can correct, not as a job error. The result
# is a BLOB so neither reader scans it for a FAILED banner.
set -euo pipefail

out=/tmp/out

answer() { printf '%s\n' "$*" > "$out"; caos put "$out" /cas/out; exit 0; }

caos get /cas/args/hash
hash=$(tr -d '[:space:]' < /cas/args/hash)
printf '%s' "$hash" | grep -qE '^[0-9a-f]{40}$' \
  || answer "not a hash: $hash

Pass the 40-character ArgTree hash the \`caos-test\` report prints in its 'full trace' line."

top=25
if [ -e /cas/args/top ]; then
  caos get /cas/args/top
  top=$(tr -d '[:space:]' < /cas/args/top)
  printf '%s' "$top" | grep -qE '^[0-9]+$' || answer "top must be a number, got: $top"
fi

# `status --all` prints nothing to stdout and a note to stderr when the server
# has no record: that is an answer, not a failure. A failure to ASK is.
if ! caos status --all "$hash" > /tmp/trace.json 2>/tmp/trace.err; then
  answer "could not read the trace of $hash:
$(cat /tmp/trace.err)"
fi
[ -s /tmp/trace.json ] \
  || answer "nothing recorded under $hash ($(cat /tmp/trace.err))

Trace records are kept per ArgTree by the server's redis; this hash has none there."

{
  echo "trace of $hash"
  echo
  echo "---- slowest $top nodes (ms relative to the root's request; reused = ran in an earlier run) ----"
  # Flatten the tree. A node's duration is ended - started; a node that never
  # started or ended (still running, or parked waiting for capacity) has none
  # and is listed last as unfinished.
  jq -r --argjson top "$top" '
    [ recurse(.children[]?) | {
        name: (.name // "?"), req: .requested, start: .started, end: .ended,
        reused: (.reused // false),
        dur: (if .started != null and .ended != null then .ended - .started else null end),
        wait: (if .requested != null and .started != null then .started - .requested else null end)
      } ]
    | (map(select(.dur != null)) | sort_by(-.dur) | .[:$top]) as $slow
    | "nodes: \(length), reused: \(map(select(.reused)) | length), unfinished: \(map(select(.end == null)) | length)",
      "   dur    wait   start  name",
      ($slow[] | "\(.dur | tostring | (" " * (6 - length)) + .)  \(.wait // "-" | tostring | (" " * (6 - length)) + .)  \(.start | tostring | (" " * (6 - length)) + .)  \(.name)\(if .reused then "  [reused]" else "" end)")
  ' /tmp/trace.json
  echo
  echo "---- full trace JSON ----"
  jq . /tmp/trace.json
} > "$out"

if [ -e /cas/args/perf ]; then
  # The distinct out-trace objects named anywhere in the tree. They are git
  # objects the workers left: fetch each by hash and print it verbatim.
  jq -r '[ recurse(.children[]?) | .out_trace[]? ] | unique | .[]' /tmp/trace.json > /tmp/oids
  {
    echo
    if [ -s /tmp/oids ]; then
      echo "---- perf data: $(wc -l < /tmp/oids) out-trace object(s) ----"
    else
      echo "---- perf data: none ----"
      echo "(out-trace is on a node only while its fan-out runs: SPEC, 'Tracing', and the"
      echo " completed view of a finished run no longer carries it. A worker that wants"
      echo " its perf data kept must also write it where a later reader finds it.)"
    fi
  } >> "$out"
  n=0
  while read -r oid; do
    n=$((n + 1))
    printf '\n-- out-trace %s --\n' "$oid" >> "$out"
    if caos get-hash "$oid" "/tmp/ot$n" 2>/tmp/ot.err; then
      if [ -d "/tmp/ot$n" ]; then
        caos get -r "/tmp/ot$n" 2>/dev/null || true
        ls -A "/tmp/ot$n" >> "$out"
        find "/tmp/ot$n" -type f -exec sh -c 'echo "## $1"; cat "$1"' _ {} \; >> "$out"
      else
        cat "/tmp/ot$n" >> "$out"
      fi
    else
      printf 'could not fetch: %s\n' "$(cat /tmp/ot.err)" >> "$out"
    fi
  done < /tmp/oids
fi

caos put "$out" /cas/out
