#!/usr/bin/env bash
# A lock, as an actor's inner (std/actor/README.md): the claim/release half of
# the lease actor that README proposes for daemons ("Deferred: daemons"). The
# state is one file, `holder`, present while the lock is held.
#
#   claim <who>    granted if the lock is free or already <who>'s, else busy
#   release <who>  give the lock up
#
# --release says what a release frees, and is the whole point of this fixture:
#
#   any     the lock, whoever holds it
#   holder  the lock only if <who> holds it
#
# Under either, applying a message twice in a row is applying it once, which is
# all std/actor's rule 4 asks. Only `holder` also survives the message being
# applied again LATER, after another client's -- std/actor's retry after a crash
# that lost the reply. tests/actor-check is the demonstration.
set -euo pipefail
caos get /cas/args/message
caos get /cas/args/release
read -r op who < /cas/args/message
mode=$(cat /cas/args/release)
caos get /cas/args/state
holder=""
if [ -e /cas/args/state/holder ]; then
  caos get /cas/args/state/holder
  holder=$(cat /cas/args/state/holder)
fi

# A warm runner reuses /tmp across jobs, so start from nothing.
rm -rf /tmp/out
mkdir -p /tmp/out/state
keep() {
  if [ -n "$holder" ]; then printf '%s\n' "$holder" > /tmp/out/state/holder; fi
}

case "$op" in
claim)
  if [ -z "$holder" ] || [ "$holder" = "$who" ]; then
    printf '%s\n' "$who" > /tmp/out/state/holder
    reply=granted
  else
    keep
    reply=busy
  fi
  ;;
release)
  case "$mode" in
  any) ;;
  holder)
    if [ "$holder" != "$who" ]; then keep; fi
    ;;
  *)
    echo "lock: unknown --release: $mode" >&2
    exit 1
    ;;
  esac
  reply=ok
  ;;
*)
  echo "lock: unknown message: $op $who" >&2
  exit 1
  ;;
esac
printf '%s\n' "$reply" > /tmp/out/reply
caos put /tmp/out /cas/out
