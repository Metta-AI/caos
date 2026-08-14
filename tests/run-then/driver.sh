#!/bin/bash
# Runs *inside* a bash worker. The generic run-then driver: tail-call a
# run-then continuation over --in, with the run (and optional then) images
# curried in as --run-img / --then-img (each a literal image ref).
set -euo pipefail
caos get /cas/args/run-img
args=(--run:hash="$(cat /cas/args/run-img)")
if [ -e /cas/args/then-img ]; then
  caos get /cas/args/then-img
  args+=(--then:hash="$(cat /cas/args/then-img)")
fi
# --catch is a bare flag: any binding of it turns it on, so the value is never
# read (the test curries --catch=1 purely because a curried arg needs one).
if [ -e /cas/args/catch ]; then
  args+=(--catch)
fi
caos run-then /cas/args/in "${args[@]}"
