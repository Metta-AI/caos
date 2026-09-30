#!/bin/bash
# One concurrent writer for tests/actor. Used as a map-then `map`, it is called
# with --in=<message blob>; it sends that message to the actor and, when the
# request loses the race for the branch (the wrapper fails it, uncached), sends
# it again with a new nonce, up to MAX attempts. Its callback is this same
# script with --attempt and --msg curried on and --result or --error supplied.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
MAX=24

if [ -e /cas/args/result ]; then
  caos forward /cas/args/result /cas/out
  exit 0
fi

attempt=0
if caos get /cas/args/attempt 2>/dev/null; then attempt=$(cat /cas/args/attempt); fi
if [ -e /cas/args/error ]; then
  caos get /cas/args/error
  attempt=$((attempt + 1))
  [ "$attempt" -lt "$MAX" ] || fail "still losing the race after $MAX attempts: $(cat /cas/args/error)"
fi

if [ -e /cas/args/msg ]; then msg=/cas/args/msg; else caos get /cas/args/in; msg=/cas/args/in; fi
caos get /cas/args/state-ref /cas/args/test-salt
state_ref=$(cat /cas/args/state-ref)
nonce="$(caos hash "$msg")-$attempt-$(cat /cas/args/test-salt)"

inner=$(caos curry --base:@=/cas/args/bash --worker1:@=/cas/args/kv)
request=$(caos prepare-request --base:@=/cas/args/actor --state-ref="$state_ref" \
  --inner:hash="$inner" --nonce="$nonce" --message:@="$msg") || fail "preparing the request"
callback=$(caos curry --base:@=/cas/args/base --worker1:@=/cas/args/worker1 \
  --bash:@=/cas/args/bash --actor:@=/cas/args/actor --kv:@=/cas/args/kv \
  --state-ref="$state_ref" --test-salt:@=/cas/args/test-salt \
  --attempt="$attempt" --msg:@="$msg")
caos run-request-then "$request" --then:hash="$callback" --catch
