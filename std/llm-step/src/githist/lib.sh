#!/usr/bin/env bash
# Embedded git-history library for the built-in `log`/`show`/`diff` tools
# (crates/worker-llm-step/src/githist.rs prepends this to each command body and
# `caos put`s the result as the worker script). NOT read from the workspace —
# these tools ship with the harness.
#
# A `#@git`-style launch gives the worker `/cas/args/wc` (the workspace commit,
# materialized as the raw commit object) and `/cas/args/refs` (the turn's
# `name <hash>` snapshot). From those two entry points every reachable object is
# one `caos get-hash` away — a commit checks out as its raw bytes, a tree as a
# directory of placeholders (expand with `caos get -r`), a blob as its content.
# So history needs no git binary: the std/bash image (bash, coreutils,
# diffutils, gnugrep, findutils, jq — NO git, NO sed/awk) is enough.
#
# Written for `set -euo pipefail`: no `[ … ] && …` tails, grep no-match guarded.
set -euo pipefail

_gh_n=0
_fresh() { _gh_n=$((_gh_n + 1)); printf '/cas/gh-%s' "$_gh_n"; }

declare -A _OBJ

githist_init() {
  if ! caos get /cas/args/wc >/dev/null 2>&1; then
    echo "githist: no workspace commit available" >&2
    return 1
  fi
  WC=$(caos hash /cas/args/wc)
  _OBJ[$WC]=/cas/args/wc
  REFS=""
  if caos get /cas/args/refs >/dev/null 2>&1; then REFS=$(cat /cas/args/refs); fi
}

obj() {
  local h=$1
  if [ -z "${_OBJ[$h]:-}" ]; then
    local p; p=$(_fresh)
    if ! caos get-hash "$h" "$p" >/dev/null 2>&1; then
      echo "object not found on this server: $h" >&2
      return 1
    fi
    _OBJ[$h]=$p
  fi
  printf '%s' "${_OBJ[$h]}"
}

obj_commit() {
  local p; p=$(obj "$1") || return 1
  if [ -d "$p" ]; then echo "$1 is a tree, not a commit" >&2; return 1; fi
  printf '%s' "$p"
}

commit_tree() {
  local p k v _rest; p=$(obj_commit "$1") || return 1
  while IFS=' ' read -r k v _rest; do
    if [ -z "$k" ]; then break; fi
    if [ "$k" = tree ]; then printf '%s' "$v"; return 0; fi
  done < "$p"
  echo "commit $1 has no tree line" >&2; return 1
}

commit_parents() {
  local p k v _rest; p=$(obj_commit "$1") || return 1
  while IFS=' ' read -r k v _rest; do
    if [ -z "$k" ]; then break; fi
    if [ "$k" = parent ]; then printf '%s\n' "$v"; fi
  done < "$p"
}

commit_first_parent() { commit_parents "$1" | head -n1; }

commit_message() {
  local p inbody=0 line; p=$(obj_commit "$1") || return 1
  while IFS= read -r line || [ -n "$line" ]; do
    if [ "$inbody" -eq 1 ]; then printf '%s\n' "$line"; continue; fi
    if [ -z "$line" ]; then inbody=1; fi
  done < "$p"
}

commit_subject() { commit_message "$1" | head -n1; }

# "name<TAB>unixts" from the author line (`author <name> <email> <ts> <tz>`).
commit_author_date() {
  local p line rest; p=$(obj_commit "$1") || return 1
  while IFS= read -r line; do
    if [ -z "$line" ]; then break; fi
    case "$line" in
      author\ *)
        rest=${line#author }
        local name=${rest%% <*}
        local rest2=${rest% *}
        local ts=${rest2##* }
        printf '%s\t%s' "$name" "$ts"
        return 0 ;;
    esac
  done < "$p"
  printf '?\t?'
}

_resolve_base() {
  local b=$1
  case "$b" in
    ''|HEAD|wc|@) printf '%s' "$WC"; return 0 ;;
  esac
  if printf '%s' "$b" | grep -qiE '^[0-9a-f]{40}([0-9a-f]{24})?$'; then
    printf '%s' "$b"; return 0
  fi
  local line
  line=$(printf '%s\n' "$REFS" | grep -E "^${b}[[:space:]]" || true)
  if [ -n "$line" ]; then printf '%s' "${line##* }"; return 0; fi
  {
    echo "cannot resolve revision: $b"
    echo "give the workspace commit (HEAD / wc), a full commit hash, or a snapshot ref:"
    if [ -n "$REFS" ]; then printf '%s\n' "$REFS" | while read -r n _; do [ -n "$n" ] && echo "  $n"; done
    else echo "  (no refs in this turn's snapshot)"; fi
  } >&2
  return 1
}

resolve_rev() {
  local spec=$1 base=$1 n=0
  case "$spec" in
    *~*) base=${spec%~*}; n=${spec##*~} ;;
    *^)  base=${spec%^};  n=1 ;;
  esac
  case "$n" in ''|*[!0-9]*) echo "bad revision suffix in: $spec" >&2; return 1 ;; esac
  local h; h=$(_resolve_base "$base") || return 1
  local i=0
  while [ "$i" -lt "$n" ]; do
    local p; p=$(commit_first_parent "$h") || return 1
    if [ -z "$p" ]; then echo "revision $spec: $h has no parent to walk to" >&2; return 1; fi
    h=$p; i=$((i + 1))
  done
  printf '%s' "$h"
}

checkout_tree() {
  local commit=$1 sub=${2:-} th dst
  th=$(commit_tree "$commit") || return 1
  dst=$(_fresh)
  caos get-hash "$th" "$dst" >/dev/null 2>&1 || { echo "tree not found: $th" >&2; return 1; }
  local target=$dst comp
  if [ -n "$sub" ]; then
    local IFS=/
    for comp in $sub; do
      if [ -z "$comp" ] || [ "$comp" = . ]; then continue; fi
      caos get "$target" >/dev/null 2>&1 || true
      if [ ! -e "$target/$comp" ]; then echo "no such path in $commit: $sub" >&2; return 1; fi
      target="$target/$comp"
    done
  fi
  caos get -r "$target" >/dev/null 2>&1 || true
  printf '%s' "$target"
}

# git oid of <path> in <commit>, or empty if absent.
path_oid() {
  local commit=$1 path=$2 th dst target comp
  th=$(commit_tree "$commit") || return 1
  dst=$(_fresh)
  caos get-hash "$th" "$dst" >/dev/null 2>&1 || return 1
  target=$dst
  local IFS=/
  for comp in $path; do
    if [ -z "$comp" ] || [ "$comp" = . ]; then continue; fi
    caos get "$target" >/dev/null 2>&1 || true
    if [ ! -e "$target/$comp" ]; then printf ''; return 0; fi
    target="$target/$comp"
  done
  caos hash "$target" 2>/dev/null || printf ''
}

_tmp_n=0
_fresh_tmp() { _tmp_n=$((_tmp_n + 1)); mkdir -p /tmp/gh; printf '/tmp/gh/f-%s' "$_tmp_n"; }

# Unified diff between two revisions, optionally scoped to a path. The temp
# checkout roots are rewritten to a/b so the headers read like git's.
diff_revs() {
  local from=$1 to=$2 sub=${3:-} A B raw rc line
  A=$(checkout_tree "$from" "$sub") || return 1
  B=$(checkout_tree "$to" "$sub") || return 1
  raw=$(_fresh_tmp)
  if diff -ruN "$A" "$B" > "$raw"; then rc=0; else rc=$?; fi
  if [ "$rc" -gt 1 ]; then echo "diff failed (rc=$rc)" >&2; return 1; fi
  while IFS= read -r line; do
    line=${line//$A/a}
    line=${line//$B/b}
    printf '%s\n' "$line"
  done < "$raw"
}

# ISO date for a unix timestamp, or the raw value if it is not one.
fmt_date() {
  case "$1" in
    ''|*[!0-9]*) printf '%s' "$1" ;;
    *) date -u -d "@$1" "+${2:-%Y-%m-%dT%H:%MZ}" 2>/dev/null || printf '%s' "$1" ;;
  esac
}
