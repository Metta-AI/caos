#!/bin/bash
# The package's worker. Deliberately deterministic and deliberately trivial:
# what is under test is that evaluating a `run`-valued expression DISPATCHES
# this, not anything this computes.
set -euo pipefail
caos get /cas/args/name
mkdir -p /tmp/out
echo "hello $(cat /cas/args/name)" > /tmp/out/greeting
caos put /tmp/out /cas/out
