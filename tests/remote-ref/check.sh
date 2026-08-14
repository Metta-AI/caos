#!/bin/bash
# Runs INSIDE a bash worker, as the `worker1` of the test's main run.
#
# Reports the RECORDED HASH of each `:@@=`-resolved arg — read off the
# placeholder, so nothing is fetched to learn it — which is the only thing the
# caller needs to check its claim: that a locator resolves to the foreign repo's
# own oid, and to the same oid a local `:@=` of the same bytes produces.
#
# It also asserts, from in here, that a WORKER cannot resolve a locator at all.
# That is the security story, not an implementation detail: resolution is a
# CLIENT capability precisely so the grammar never becomes a way for a sandboxed
# worker to reach the network (design/flake-inputs.md).
set -euo pipefail

out=/tmp/report
: > "$out"
for name in tree file whole local; do
  printf '%s %s\n' "$name" "$(caos hash "/cas/args/$name")" >> "$out"
done

# Not just resolved — DELIVERED. The oids came from a repo the server has never
# heard of, so reading the content here proves the client pushed the fetched
# closure along with the request, exactly as it does for a local path arg.
caos get -r /cas/args/tree
printf 'note %s\n' "$(cat /cas/args/tree/note.txt)" >> "$out"

# A well-formed locator, refused for WHO is asking rather than for what it says:
# the host is never contacted (`example.invalid` is reserved as unresolvable, so
# a regression that did try to fetch fails here too, just more slowly).
rev=$(printf 'a%.0s' {1..40})
if caos curry --base:@=/cas/args/base "--x:@@=git+https://example.invalid/r?rev=$rev" \
     >/dev/null 2>/tmp/err; then
  echo "FAIL: a worker resolved a remote ref" >&2
  exit 1
fi
if ! grep -q 'CLIENT capability' /tmp/err; then
  echo "FAIL: a worker's :@@= failed for the wrong reason: $(cat /tmp/err)" >&2
  exit 1
fi
printf 'worker-refused ok\n' >> "$out"

caos put "$out" /cas/out
