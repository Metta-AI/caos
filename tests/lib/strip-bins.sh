#!/bin/bash
# The `then` of the build worker: given the cargo result (--result), fail
# loudly if the build failed, else return just its bin tree. The symlink put
# reuses the recorded hash — no bytes are copied or re-read.
set -euo pipefail

caos get /cas/args/result
caos get /cas/args/result/exit
if [ "$(cat /cas/args/result/exit)" != 0 ]; then
  caos get /cas/args/result/stderr || true
  tail -60 /cas/args/result/stderr >&2 || true
  echo "BUILD: workspace build failed" >&2
  exit 1
fi
ln -s /cas/args/result/bin /tmp/binlink
caos put /tmp/binlink /cas/out
