#!/bin/bash
# SPIKE (design/actors.md, open question 6): can finish push a child commit
# without downloading the branch's whole history? Builds a 40-commit branch on
# the server, then for each variant starts from an EMPTY partial-clone scratch
# repo, fetches only the head, and tries to push a child of it. Each variant
# reports whether the push worked and how many commits it had to hold locally.
# The test always fails at the end so that the report is printed.
set -uo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

: "${CAOS_SERVER_URL:?needs CAOS_SERVER_URL from the runner}"
caos get /cas/args/test-salt || fail "reading --test-salt"
SALT=$(cat /cas/args/test-salt)
N=40
REPORT=""
note() { REPORT="$REPORT$*"$'\n'; }

# A 40-commit chain on a fresh server branch; prints the branch ref.
mkchain() { # <variant>
  local ref="refs/heads/actors/parents-$SALT-$1-$(date +%s%N)-$RANDOM" prev="" i blob sub root c
  rm -rf /tmp/setup
  mkdir -p /tmp/setup
  (
    cd /tmp/setup || exit 1
    git init -q .
    git config user.email t@caos
    git config user.name t
    git remote add caos "$CAOS_SERVER_URL"
    for i in $(seq 1 $N); do
      blob=$(printf '%s\n' "$i" | git hash-object -w --stdin)
      sub=$(printf '100644 blob %s\tv\n' "$blob" | git mktree)
      root=$(printf '040000 tree %s\tstate\n' "$sub" | git mktree)
      if [ -n "$prev" ]; then c=$(git commit-tree "$root" -p "$prev" -m "c$i"); else c=$(git commit-tree "$root" -m "c$i"); fi
      prev=$c
    done
    git push -q caos "$prev:$ref" || exit 1
    echo "$prev" > /tmp/setup-head
  ) || fail "building the chain for $1"
  printf '%s\n' "$ref"
}

scratch() { # <dir>: an empty partial-clone scratch repo, like std/actor's
  rm -rf "$1"
  mkdir -p "$1"
  cd "$1" || exit 1
  git init -q --bare .
  git config user.email t@caos
  git config user.name t
  git config gc.auto 0
  git remote add origin "$CAOS_SERVER_URL"
  git config core.repositoryformatversion 1
  git config extensions.partialClone origin
  git config remote.origin.promisor true
  git config remote.origin.partialclonefilter tree:0
}

local_commits() { git cat-file --batch-all-objects --batch-check 2>/dev/null | grep -c ' commit '; }

# child <head>: a commit on <head> whose state subtree is brand new.
child() {
  local blob sub root
  blob=$(printf 'next\n' | git hash-object -w --stdin)
  sub=$(printf '100644 blob %s\tv\n' "$blob" | git mktree)
  root=$(printf '040000 tree %s\tstate\n' "$sub" | git mktree)
  git commit-tree "$root" -p "$1" -m "child"
}

variant() { # <name> <fetch args...>; then optionally VARIANT_HOOK runs before the push
  local name=$1 ref head c out
  shift
  ref=$(mkchain "$name")
  head=$(cat /tmp/setup-head)
  scratch "/tmp/scratch-$name"
  if ! git fetch -q --no-tags --no-write-fetch-head "$@" origin "$head" 2>/tmp/err; then
    note "$name: fetch FAILED: $(tr '\n' ' ' < /tmp/err)"
    return
  fi
  if [ -n "${VARIANT_HOOK:-}" ]; then eval "$VARIANT_HOOK"; fi
  c=$(child "$head") || { note "$name: could not write the child commit"; return; }
  if out=$(git push ${PUSH_EXTRA:-} --force-with-lease="$ref:$head" origin "$c:$ref" 2>&1); then
    note "$name: PUSH OK; commits held locally: $(local_commits) of $N"
  else
    note "$name: PUSH FAILED ($(printf '%s' "$out" | tr '\n' ' ' | cut -c1-300)); commits held locally: $(local_commits) of $N"
  fi
}

# 1. Baseline, what std/actor does now: the head fetched without --depth.
VARIANT_HOOK="" variant full --filter=tree:0
# 2. Depth 1 and leave the shallow file in place.
VARIANT_HOOK="" variant shallow --depth=1 --filter=tree:0
# 3. Depth 1, then drop the shallow file: the head's parent is "promised" (the
#    head came in a promisor pack) rather than present.
VARIANT_HOOK='rm -f shallow' variant noshallow --depth=1 --filter=tree:0

# 4. As 3, plus a diagnostic: is the depth-1 pack marked as a promisor pack?
VARIANT_HOOK='note "  packs: $(ls objects/pack | tr "\n" " ")"; note "  fsck-ish: $(git rev-list --missing=allow-promisor --count --all 2>&1 | tr "\n" " ")"; rm -f shallow' variant diag --depth=1 --filter=tree:0
# 5. As 3, pushed with --no-thin.
VARIANT_HOOK='rm -f shallow; PUSH_EXTRA=--no-thin' variant nothin --depth=1 --filter=tree:0

fail $'report\n'"$REPORT"
