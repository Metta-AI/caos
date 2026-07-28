#!/bin/bash
# Warm the std memos, as the test stack's worker1 (design/test-stack-image.md).
# Runs ONE test stack, publishes its inner std, and exits — the point is the
# side effect: every std image the tree defines ends up in the host registry
# under its content-keyed tag.
#
# Why this exists, measured the hard way. Publishing std per test is correct
# and cheap against a WARM registry (~7 min for 19 tests). Against a cold one
# it is a thundering herd: all nineteen stacks start together, all miss the
# same memo, and all start their own toolchain bake. Each test stack is an
# outer worker holding an outer runner slot for its whole life, so the pool
# fills with 20-minute jobs and whatever is still queued dies on the pending
# timeout — `no runner for req (waited 900s)`, observed after std/refresh.sh
# re-keyed the std/cargo tree.
#
# Serializing one publish first turns every later one into a tag hit. Note
# this is NOT the digest-passing scheme design/test-stack-image.md first
# proposed: the tests still publish their own std, built by their own stack.
# The registry is already the sharing mechanism; it just has to be warm.
set -euo pipefail

CAOS_CLI=/caos/bin/caos-cli \
CAOS_CLIENT_REPO=/tmp/publish-client-repo \
CAOS_BUILTIN_IMAGES="$(echo /caos/images/*.tar.gz)" \
CAOS_BUILTIN_BINS=/caos \
CAOS_REGISTRY_HTTP=caos-registry:5000 \
  bash /caos/tree/build-builtins.sh >/tmp/publish.log 2>&1 || {
  echo "WARM-STD FAIL: publishing std" >&2
  cat /tmp/publish.log >&2
  exit 1
}

mkdir -p /tmp/out
cp /tmp/publish.log /tmp/out/publish.log
grep -E "^(runner|flake-builder|cargo|bash|rustc):" /tmp/publish.log \
  > /tmp/out/report || true
cat /tmp/out/report >&2
