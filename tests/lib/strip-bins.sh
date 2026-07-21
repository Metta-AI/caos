#!/bin/bash
# The `then` of build-bins.sh: given the cargo build's result (--result), fail
# loudly if the build failed, else return just its bin tree. The symlink put
# reuses the recorded hash — no bytes are copied or re-read.
set -euo pipefail

# Expand the result one level (its children are placeholders until then).
caos get /cas/args/result
caos get /cas/args/result/exit
if [ "$(cat /cas/args/result/exit)" != 0 ]; then
  caos get /cas/args/result/stderr || true
  tail -60 /cas/args/result/stderr >&2 || true
  echo "BUILD-BINS: workspace build failed" >&2
  exit 1
fi
ln -s /cas/args/result/bin /tmp/binlink
caos put /tmp/binlink /cas/out
