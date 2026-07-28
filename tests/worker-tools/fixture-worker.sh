#!/usr/bin/env bash
# A generic worker-tool fixture. It reads only the standardized input and
# returns only the standardized result envelope.
set -euo pipefail

caos get /cas/args/in
caos get /cas/args/workspace
caos get /cas/args/workspace/marker.txt

call=$(cat /cas/args/in)
message=${call#*'"message":"'}
message=${message%%'"'*}
marker=$(cat /cas/args/workspace/marker.txt)
mkdir /tmp/result
printf '{"content":"fixture saw %s and %s","is_error":false}\n' \
  "$message" "$marker" > /tmp/result/result.json
caos put /tmp/result /cas/out
