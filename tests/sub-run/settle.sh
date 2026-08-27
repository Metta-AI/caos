#!/bin/bash
# Runs *inside* a bash worker: sleep, and return the marker it was given.
#
# THE TEST'S WAY OF WAITING WITHOUT WAITING ON ANYTHING. tests/sub-run has to
# let wall-clock pass while NOTHING observes the dispatched job — that is the
# claim — and a worker must not block on another job. So the delay is a job of
# its own: it depends on nothing, always finishes, and always frees its slot, so
# no amount of it can deadlock a suite. `--marker` keys it to this run, since a
# cached sleep would return instantly and defeat the point.
set -euo pipefail
caos get /cas/args/marker
caos get /cas/args/seconds
sleep "$(cat /cas/args/seconds)"
caos forward /cas/args/marker /cas/out
