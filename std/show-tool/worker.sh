#!/usr/bin/env bash
# The `show` tool. The shared history library arrives as `lib` from
# std/githist-lib (this entry curries onto it), and the command body below
# is everything specific to this tool.
#
# An arg is a PLACEHOLDER until it is fetched, so the library has to be
# `caos get`-ed before it can be sourced.
set -euo pipefail
caos get /cas/args/lib >/dev/null
. /cas/args/lib

# ---- show -----------------------------------------------------------------
githist_init

rev=HEAD
if caos get /cas/args/rev >/dev/null 2>&1; then rev=$(cat /cas/args/rev); fi
path=""
if caos get /cas/args/path >/dev/null 2>&1; then path=$(cat /cas/args/path); fi

h=$(resolve_rev "$rev")
out=/tmp/out

{
  printf 'commit  %s\n' "$h"
  parents=$(commit_parents "$h" | tr '\n' ' ')
  printf 'parent  %s\n' "${parents:-(root commit)}"
  IFS=$'\t' read -r name ts <<< "$(commit_author_date "$h")"
  printf 'author  %s  %s\n' "$name" "$(fmt_date "$ts" '%Y-%m-%dT%H:%M:%SZ')"
  echo
  commit_message "$h" | while IFS= read -r line; do printf '    %s\n' "$line"; done
  echo

  first=$(commit_first_parent "$h")
  if [ -z "$first" ]; then
    echo "(root commit — no parent to diff against)"
  else
    printf -- '---- diff %s..%s%s ----\n' "${first:0:12}" "${h:0:12}" "${path:+ ($path)}"
    diff_revs "$first" "$h" "$path"
  fi
} > "$out"

caos put "$out" /cas/out
