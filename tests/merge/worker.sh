#!/bin/bash
# tests/merge — a WORKER test: no client, no repo.
#
# Drives the std/merge worker directly (SPEC "Merging and conflict
# resolution"): build two commits, merge them, and assert the two-parent result
# — a conflicted merge (inline markers + .caos/conflicts) and a clean one. The
# git-bearing worker fetches both commit closures from the server's own
# transport, so this also exercises that path.
#
# GIT WAS A LOCAL TOOL HERE, NOT A SUBJECT, which is why this needs none.
# `caos put-commit` mints a commit — the raw object, validated client-side and
# again by the server — and a `commit`-kind RESULT is materialized as those same
# raw bytes, so `tree <oid>` and `parent <oid>` are lines to grep and the tree
# is one `caos get-hash` away. That is the whole of what `git rev-parse M^` and
# `git show M:f.txt` were doing.
#
# FIXED AUTHOR TIME, salted MESSAGE. The timestamp is a constant because one
# assertion is that M inherits ours' — a real check only if the value is
# pinned — and because a later stage re-mints the same commits and must get the
# same oids. The message carries `--test-salt` so a salted run genuinely
# recomputes rather than replaying the merge it did last time.
#
# FOUR STAGES: no run can be waited on, so each assertion is the `then` of the
# merge it is about.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --merge:@=/cas/args/merge "$@"; }

caos get /cas/args/test-salt || fail "reading --test-salt"
SALT=$(cat /cas/args/test-salt)
TS=1700000000

# A tree from a list of `<path>:<content>` pairs -> its oid.
mktree() { # <cas-name> <path=content>...
  local dst=$1; shift
  local r=/tmp/t; rm -rf "$r"; mkdir -p "$r"
  local pair
  for pair in "$@"; do
    printf '%s\n' "${pair#*=}" > "$r/${pair%%=*}"
  done
  caos put "$r" "/cas/$dst" >/dev/null || fail "publishing tree $dst"
  caos hash "/cas/$dst"
}

# A commit, minted as raw bytes. `put-commit` prints its hash and records it at
# a commit-typed CAS path, which is what makes `--name:@=` carry it as a gitlink.
mint() { # <cas-name> <tree-oid> <message> [parent...]
  local dst=$1 tree=$2 msg=$3; shift 3
  local p
  { printf 'tree %s\n' "$tree"
    for p in "$@"; do printf 'parent %s\n' "$p"; done
    printf 'author caos <test@caos> %s +0000\n' "$TS"
    printf 'committer caos <test@caos> %s +0000\n' "$TS"
    printf '\n%s (%s)\n' "$msg" "$SALT"
  } > /tmp/commit
  caos put-commit /tmp/commit "/cas/$dst" || fail "minting $dst"
}

# The merged commit's raw bytes are the result. Everything the old test asked
# git is a line in them, or in the tree they name.
merge_run() { # <ours-cas-path> <theirs-cas-path> -> a request hash
  caos prepare-request --base:hash="$(caos hash /cas/args/merge)" \
    --ours:@="$1" --theirs:@="$2"
}
result_commit() { caos get /cas/args/result >/dev/null; cat /cas/args/result; }
commit_field() { printf '%s\n' "$1" | grep -m1 "^$2 " | cut -d' ' -f2- ; }
# The merged tree, materialized: `caos get-hash` fetches by oid, `-r` walks it.
merged_tree() { # <raw commit> -> a path
  local t; t=$(commit_field "$1" tree)
  rm -rf /cas/m 2>/dev/null || true
  caos get-hash "$t" /cas/m || fail "fetching the merged tree $t"
  caos get -r /cas/m || fail "materializing the merged tree"
  echo /cas/m
}

# base: f.txt (three lines), keep.txt (untouched by either side).
# ours: line2 one way, plus a clean add. theirs: the SAME line differently,
# plus a different clean add. So one conflict and two clean adds.
build_sides() {
  BASE_T=$(mktree base-t "f.txt=line1
line2
line3" "keep.txt=keep")
  OURS_T=$(mktree ours-t "f.txt=line1
OURS
line3" "keep.txt=keep" "ours.txt=o")
  THEIRS_T=$(mktree theirs-t "f.txt=line1
THEIRS
line3" "keep.txt=keep" "theirs.txt=t")
  BASE=$(mint base "$BASE_T" base)
  OURS=$(mint ours "$OURS_T" ours "$BASE")
  THEIRS=$(mint theirs "$THEIRS_T" theirs "$BASE")
}

case "$stage" in

start)
  echo "== build a base and two diverging sides ==" >&2
  build_sides
  echo "  base=$BASE ours=$OURS theirs=$THEIRS" >&2

  echo "== a conflicting merge: two parents, markers, .caos/conflicts ==" >&2
  caos run-request-then "$(merge_run /cas/ours /cas/theirs)" \
    --then:hash="$(next conflicted --ours="$OURS" --theirs="$THEIRS" --base-c="$BASE")"
  ;;

conflicted)
  caos get /cas/args/ours; caos get /cas/args/theirs; caos get /cas/args/base-c
  ours=$(cat /cas/args/ours); theirs=$(cat /cas/args/theirs)
  m=$(result_commit)
  [ "$(commit_field "$m" tree)" != "" ] || fail "the result is not a commit: $m"
  parents=$(printf '%s\n' "$m" | grep '^parent ' | cut -d' ' -f2)
  [ "$(printf '%s\n' "$parents" | head -1)" = "$ours" ] \
    || fail "M's first parent is not ours: $parents"
  [ "$(printf '%s\n' "$parents" | tail -1)" = "$theirs" ] \
    || fail "M's second parent is not theirs: $parents"
  # ours' author time is the fixed TS above, so this is a real inheritance check.
  case "$(commit_field "$m" committer)" in
    *" $TS "*) ;;
    *) fail "M did not inherit ours' timestamp: $(commit_field "$m" committer)" ;;
  esac
  echo "  ok: [ours, theirs] parents, ours' timestamp" >&2

  t=$(merged_tree "$m")
  f=$(cat "$t/f.txt")
  printf '%s' "$f" | grep -q '<<<<<<<' || fail "no conflict marker in f.txt:
$f"
  printf '%s' "$f" | grep -q 'OURS'   || fail "ours side missing from f.txt"
  printf '%s' "$f" | grep -q 'THEIRS' || fail "theirs side missing from f.txt"
  [ "$(cat "$t/ours.txt")" = "o" ]      || fail "ours.txt not merged in"
  [ "$(cat "$t/theirs.txt")" = "t" ]    || fail "theirs.txt not merged in"
  [ "$(cat "$t/keep.txt")" = "keep" ]   || fail "untouched keep.txt changed"

  # .caos/conflicts is the authoritative set — git's unmerged notation, naming
  # the conflicted path (and only it).
  caos get -r "$t/.caos" || fail "M has no .caos"
  conflicts=$(cat "$t/.caos/conflicts") || fail "M has no .caos/conflicts"
  printf '%s' "$conflicts" | grep -q 'f.txt' || fail ".caos/conflicts does not name f.txt:
$conflicts"
  if printf '%s' "$conflicts" | grep -q 'ours.txt'; then
    fail ".caos/conflicts names the clean ours.txt"
  fi
  echo "  ok: markers in f.txt; .caos/conflicts lists f.txt and nothing else" >&2

  echo "== an identical merge is a cache hit with the same commit ==" >&2
  build_sides
  caos run-request-then "$(merge_run /cas/ours /cas/theirs)" \
    --then:hash="$(next cached --was="$(caos hash /cas/args/result)" \
      --ours="$ours" --base-c="$(cat /cas/args/base-c)")"
  ;;

cached)
  caos get /cas/args/was; caos get /cas/args/ours; caos get /cas/args/base-c
  [ "$(caos hash /cas/args/result)" = "$(cat /cas/args/was)" ] \
    || fail "identical merge produced a different commit"
  echo "  ok: same two commits -> same M" >&2

  echo "== a clean merge: two parents, no .caos entry ==" >&2
  # theirs-clean touches only theirs.txt, never f.txt, so merging with ours is
  # conflict-free.
  build_sides
  CLEAN_T=$(mktree clean-t "f.txt=line1
line2
line3" "keep.txt=keep" "theirs.txt=tc")
  CLEAN=$(mint theirs-clean "$CLEAN_T" theirs-clean "$(cat /cas/args/base-c)")
  caos run-request-then "$(merge_run /cas/ours /cas/theirs-clean)" \
    --then:hash="$(next clean --ours="$(cat /cas/args/ours)" --theirs="$CLEAN")"
  ;;

clean)
  caos get /cas/args/ours; caos get /cas/args/theirs
  m=$(result_commit)
  parents=$(printf '%s\n' "$m" | grep '^parent ' | cut -d' ' -f2)
  [ "$(printf '%s\n' "$parents" | head -1)" = "$(cat /cas/args/ours)" ] \
    || fail "clean M's first parent is not ours"
  [ "$(printf '%s\n' "$parents" | tail -1)" = "$(cat /cas/args/theirs)" ] \
    || fail "clean M's second parent wrong"
  t=$(merged_tree "$m")
  [ ! -e "$t/.caos" ] || fail "clean merge left a .caos entry"
  [ "$(cat "$t/f.txt")" = "$(printf 'line1\nOURS\nline3')" ] \
    || fail "clean merge did not keep ours' f.txt"
  [ "$(cat "$t/ours.txt")" = "o" ]     || fail "clean merge dropped ours.txt"
  [ "$(cat "$t/theirs.txt")" = "tc" ]  || fail "clean merge dropped theirs.txt"
  echo "  ok: clean merge is a pure two-parent commit, no .caos/conflicts" >&2

  printf 'merge: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
