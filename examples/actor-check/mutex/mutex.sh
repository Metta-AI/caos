#!/usr/bin/env bash
# The lock's safety property, as a std/actor-check invariant: at most one client
# is in its critical section. A client enters when its claim is acknowledged as
# granted, and leaves when its release has been APPLIED -- not when it hears
# back, because a release whose reply was lost has still happened.
set -euo pipefail
caos get /cas/args/in
caos get /cas/args/in/clients
holding=()
for dir in /cas/args/in/clients/*; do
  caos get "$dir"
  for f in acked pending lost; do caos get "$dir/$f"; done
  held=no
  while IFS= read -r line; do
    case "$line" in
    "claim "*" => granted") held=yes ;;
    "release "*) held=no ;;
    esac
  done < "$dir/acked"
  pending=$(cat "$dir/pending")
  if [ "$held" = yes ] && [ "${pending%% *}" = release ] && [ "$(cat "$dir/lost")" -gt 0 ]; then
    held=no
  fi
  if [ "$held" = yes ]; then holding+=("${dir##*/}"); fi
done

rm -f /tmp/verdict
if [ "${#holding[@]}" -gt 1 ]; then
  printf 'violation: %d clients are in the critical section at once: %s\n' \
    "${#holding[@]}" "${holding[*]}" > /tmp/verdict
else
  printf 'ok\n' > /tmp/verdict
fi
caos put /tmp/verdict /cas/out
