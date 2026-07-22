#!/bin/bash
# The caos runner runs us as /worker, with /cas set up and the args
# materialized under /cas/args. Fetch the script and run it; on exit
# caos reads the hash of /cas/out. If the script left no result there,
# store an empty blob so there's something to read.
set -euo pipefail
caos get /cas/args/script
bash /cas/args/script
if [ ! -e /cas/out ]; then
  : > /tmp/caos-empty-out
  caos put /tmp/caos-empty-out /cas/out
fi
