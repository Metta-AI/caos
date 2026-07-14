#!/bin/bash
# Runs *inside* a bash worker, in the `run` position: --in is a numeric blob;
# the result is its double.
set -euo pipefail
caos get /cas/args/in
n=$(cat /cas/args/in)
echo $((n * 2)) > /tmp/double
caos put /tmp/double /cas/out
