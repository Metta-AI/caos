#!/bin/bash
# Runs *inside* a bash worker, in the `map` position. Sleeps for --hold seconds
# so the run is still going when the test asks /status what is happening — the
# whole point being that a live view can only be asserted while there is life.
set -euo pipefail
caos get /cas/args/in
caos get /cas/args/hold
sleep "$(cat /cas/args/hold)"
cat /cas/args/in > /tmp/out
caos put /tmp/out /cas/out
