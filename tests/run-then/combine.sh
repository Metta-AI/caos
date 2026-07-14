#!/bin/bash
# Runs *inside* a bash worker, in the `then` position: the server passes the
# original --in plus the run step's result as --result. Prove both arrived.
set -euo pipefail
caos get /cas/args/in
caos get /cas/args/result
echo "in=$(cat /cas/args/in) result=$(cat /cas/args/result)" > /tmp/combined
caos put /tmp/combined /cas/out
