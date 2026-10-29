#!/bin/bash
# Runs *inside* a bash worker. A CHAIN OF THENS: with --n above zero, promise a
# `then` that is this worker again with --n one lower (a plain tail call: no
# middle step, `in` passed through), and exit; at zero, write the value. Every
# hop is a distinct request (its --n differs), so none is a cache hit and none a
# cycle, and the server runs the whole chain for the one request that started it.
#
# Each hop used to be a native frame on the server's request thread, about 13 KB,
# so a chain longer than the thread's 2 MiB stack allowed (about 150 hops)
# aborted the whole server (design/promise-chains.md). The length tests/run-then
# asks for is chosen to be well past that; it passes only if a hop costs no
# native stack.
set -euo pipefail
caos get /cas/args/n
n=$(cat /cas/args/n)
if [ "$n" -gt 0 ]; then
  next=$(caos curry --base:@=/cas/args/base --worker1:@=/cas/args/worker1 --n=$((n - 1)))
  caos map-then /cas/args/in --then:hash="$next"
else
  caos get /cas/args/in
  echo "bottom in=$(cat /cas/args/in)" > /tmp/bottom
  caos put /tmp/bottom /cas/out
fi
