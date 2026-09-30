#!/bin/bash
# An IMPURE inner, for tests only: it does what kv.sh does, after a side effect
# on the server that the test then observes. Real inners must be pure.
#   --race-ref=R   if R does not exist yet, push a competing commit to it, so the
#                  wrapper's leased push (which observed no head) loses the race
#   --count-ref=C  push one new commit to C per execution, so the number of
#                  commits on C is the number of times this inner actually ran
set -euo pipefail

: "${CAOS_SERVER_URL:?needs CAOS_SERVER_URL from the runner}"
race_ref=""
count_ref=""
if caos get /cas/args/race-ref 2>/dev/null; then race_ref=$(cat /cas/args/race-ref); fi
if caos get /cas/args/count-ref 2>/dev/null; then count_ref=$(cat /cas/args/count-ref); fi

rm -rf /tmp/probe
mkdir -p /tmp/probe
cd /tmp/probe
git init -q .
git config user.email probe@caos
git config user.name probe
git config gc.auto 0
git remote add caos "$CAOS_SERVER_URL"

head_of() {
  local line
  line=$(git ls-remote --refs caos "$1") || return 1
  if [ -n "$line" ]; then printf '%s\n' "${line%%[[:space:]]*}"; fi
}

if [ -n "$race_ref" ] && [ -z "$(head_of "$race_ref")" ]; then
  blob=$(printf '0\n' | git hash-object -w --stdin)
  sub=$(printf '100644 blob %s\tx\n' "$blob" | git mktree)
  root=$(printf '040000 tree %s\tstate\n' "$sub" | git mktree)
  winner=$(git commit-tree "$root" -m "competing writer")
  git push -q --force-with-lease="$race_ref:" caos "$winner:$race_ref"
fi

if [ -n "$count_ref" ]; then
  empty=$(git mktree < /dev/null)
  prior=$(head_of "$count_ref")
  if [ -n "$prior" ]; then
    git fetch -q caos "$prior"
    run=$(git commit-tree "$empty" -p "$prior" -m "ran $(date +%s%N)-$RANDOM")
  else
    run=$(git commit-tree "$empty" -m "ran $(date +%s%N)-$RANDOM")
  fi
  git push -q --force-with-lease="$count_ref:$prior" caos "$run:$count_ref"
fi

caos get /cas/args/kv
exec bash /cas/args/kv
