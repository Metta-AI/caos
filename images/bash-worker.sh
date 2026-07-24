#!/usr/bin/env bash
# (env shebang: the flake bases this runs on (bash-base, testenv-base) have
# bash at /bin/bash, but the nix-builder images have only the nix profile's —
# /usr/bin/env, which the flake-builder's caos delta guarantees, is the
# portable meeting point.)
# The caos runner runs us as /worker, with /cas set up and the args
# materialized under /cas/args. We are a script interpreter: fetch the next
# executable in the chain (`worker1` — the workerN naming for anything that
# gets fetched and run) and run it with bash; on exit caos reads the hash of
# /cas/out. If the script left no result there, store an empty blob so
# there's something to read.
set -euo pipefail
caos get /cas/args/worker1
bash /cas/args/worker1
if [ ! -e /cas/out ]; then
  : > /tmp/caos-empty-out
  caos put /tmp/caos-empty-out /cas/out
fi
