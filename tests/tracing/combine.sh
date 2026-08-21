#!/bin/bash
# Runs *inside* a bash worker, in the `then` position: the map's results arrive
# under --children, by the map entry's name. Concatenates them, sorted, so the
# result is deterministic.
set -euo pipefail
# The arg is a LAZY PLACEHOLDER until it is fetched, so the glob below sees
# nothing without this and passes the literal `…/children/*` on to `caos get`.
caos get /cas/args/children
: > /tmp/combined
for c in /cas/args/children/*; do
  caos get "$c"
  printf '%s=%s\n' "$(basename "$c")" "$(cat "$c")" >> /tmp/combined
done
sort -o /tmp/combined /tmp/combined
caos put /tmp/combined /cas/out
