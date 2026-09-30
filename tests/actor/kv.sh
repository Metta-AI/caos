#!/usr/bin/env bash
# Reference inner actor (design/actors.md): a key-value store in plain bash, run
# in std/bash, which is not a runner image. A pure function of
# (state tree, message) -> {state, reply}. Messages are idempotent:
#   put <key> <value>   set key (applying twice is the same as once)
#   get <key>           reply with the value, state unchanged
set -euo pipefail

caos get /cas/args/message
read -r op key value < /cas/args/message
case "$key" in
  ''|*/*|.*) echo "kv: bad key: $key" >&2; exit 1 ;;
esac

# List the state's entries without reading their content.
caos get /cas/args/state

rm -rf /tmp/out
mkdir -p /tmp/out
case "$op" in
put)
  mkdir /tmp/out/state
  for entry in /cas/args/state/*; do
    [ -e "$entry" ] || continue
    name=$(basename "$entry")
    if [ "$name" != "$key" ]; then ln -s "$entry" "/tmp/out/state/$name"; fi
  done
  printf '%s\n' "$value" > "/tmp/out/state/$key"
  printf 'ok\n' > /tmp/out/reply
  ;;
get)
  ln -s /cas/args/state /tmp/out/state
  if [ -e "/cas/args/state/$key" ]; then
    caos get "/cas/args/state/$key"
    cp "/cas/args/state/$key" /tmp/out/reply
  else
    : > /tmp/out/reply
  fi
  ;;
*) echo "kv: unknown op: $op" >&2; exit 1 ;;
esac
caos put /tmp/out /cas/out
