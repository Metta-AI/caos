#!/bin/bash
# Runs *inside* a bash worker of the dev stack: the positive half of the
# world guard's test. Reaching /cas/out at all means this worker's client and
# the inner server agreed on their world.
set -euo pipefail
echo "world-guard ok" > /tmp/out
caos put /tmp/out /cas/out
