#!/bin/bash
# Runs *inside* a bash worker. Ties a knot: re-curry *this* worker (the image
# at /cas/args/base plus this very script) — currying is content-addressed, so
# the curry node is identical to the one the caller ran — and run-then it over
# the same --in. The sub-request is byte-identical to the in-flight one, so the
# server's run-cycle detection must fail the run.
set -euo pipefail
me=$(caos curry --base:@=/cas/args/base --worker1:@=/cas/args/worker1)
caos run-then /cas/args/in --run:hash="$me"
