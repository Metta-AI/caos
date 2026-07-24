#!/bin/sh
# This image's /worker. caos is at /bin/caos (the caos additions, stacked on
# by the flake-builder — the only thing this flake doesn't define).
echo "hello from a self-contained flake worker" > /tmp/out
/bin/caos put /tmp/out /cas/out
