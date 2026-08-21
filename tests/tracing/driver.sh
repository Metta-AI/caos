#!/bin/bash
# Runs *inside* a bash worker. Map-thens over --in with the map (and optional
# then) images curried in, so the test can build a promise whose children are
# NAMED — the map entry's name is what the trace records and what /status
# prepends to the child's own name.
#
# It also leaves perf data at /cas/out-trace before tail-calling. That is the
# one place out-trace is observable through /status: it arrives with this
# worker's result, and a node whose work is wholly done is skipped by the walk
# — but a node that left a promise is not done until its continuation is.
set -euo pipefail
caos get /cas/args/map-img
args=(--map:hash="$(cat /cas/args/map-img)")
if [ -e /cas/args/then-img ]; then
  caos get /cas/args/then-img
  args+=(--then:hash="$(cat /cas/args/then-img)")
fi
echo "driver-fanned-out" > /tmp/perf
caos put /tmp/perf /cas/out-trace
caos map-then /cas/args/in "${args[@]}"
