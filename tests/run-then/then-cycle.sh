#!/bin/bash
# Runs *inside* a bash worker. Ties a knot through `then`: the hop with --n=0
# promises a `then` that is this worker with --n=1, and that one promises --n=0
# again. Currying is content-addressed and the server threads the same `in` and
# salt into every `then`, so the third hop's request is byte-identical to the
# first's, which is still pending on it. The server's run-cycle detection must
# fail the chain, listing both hops, exactly as it did when hops were nested.
set -euo pipefail
caos get /cas/args/n
n=$(cat /cas/args/n)
other=$(caos curry --base:@=/cas/args/base --worker1:@=/cas/args/worker1 --n=$((1 - n)))
caos map-then /cas/args/in --then:hash="$other"
