#!/bin/bash
# Runs *inside* a bash worker, as the commit worker's run-then tool sub-run:
# double the numeric --in blob.
set -euo pipefail
caos get /cas/args/in
n=$(cat /cas/args/in)
echo $((n * 2)) > /tmp/tool-out
caos put /tmp/tool-out /cas/out
