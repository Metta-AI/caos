#!/usr/bin/env bash
# The `caos-stack` tool: send one message to the stack daemon for this tree.
#
# The same router as std/caos-test's, with the op and its arguments passed
# through. See that script for why the tree is both `affinity` and `in`, and
# dev/test-stack/worker.sh for what each op does.
set -euo pipefail

caos get /cas/args/in
caos get /cas/args/in/dev 2>/dev/null || true
if [ ! -f /cas/args/in/flake.nix ] || [ ! -e /cas/args/in/dev/stack-up ]; then
  { echo "caos-stack runs a caos dev stack built from the tree. This source tree is"
    echo "not the caos codebase (no flake.nix / dev/stack-up), so there is no stack"
    echo "for it to start or inspect."
    echo "caos-stack is specific to the caos codebase; run it there."
  } > /tmp/not-caos
  caos put /tmp/not-caos /cas/out
  exit 0
fi

caos get /cas/args/op
op=$(cat /cas/args/op)
case "$op" in
  start | status | logs | harvest | stop) ;;
  *)
    printf 'unknown op %s: use one of start, status, logs, harvest, stop\n' "$op" > /tmp/bad-op
    caos put /tmp/bad-op /cas/out
    exit 0
    ;;
esac

tree=$(caos hash /cas/args/in)
forward=()
for name in request-id relay advertise log cursor refs; do
  if [ -e "/cas/args/$name" ]; then forward+=("--$name:@=/cas/args/$name"); fi
done

request=$(caos prepare-request --base:@=/cas/args/daemon \
  "--affinity=$tree" "--op=$op" --in:@=/cas/args/in "${forward[@]}")
caos run-request-then "$request"
