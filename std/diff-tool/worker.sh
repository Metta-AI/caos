#!/usr/bin/env bash
# The `diff` tool. The shared history library arrives as `lib` from
# std/githist-lib (this entry curries onto it), and the command body below
# is everything specific to this tool.
#
# An arg is a PLACEHOLDER until it is fetched, so the library has to be
# `caos get`-ed before it can be sourced.
set -euo pipefail
caos get /cas/args/lib >/dev/null
. /cas/args/lib

# ---- diff -----------------------------------------------------------------
githist_init

to=HEAD
if caos get /cas/args/to >/dev/null 2>&1; then to=$(cat /cas/args/to); fi
path=""
if caos get /cas/args/path >/dev/null 2>&1; then path=$(cat /cas/args/path); fi

to_h=$(resolve_rev "$to")

if caos get /cas/args/from >/dev/null 2>&1; then
  from_h=$(resolve_rev "$(cat /cas/args/from)")
else
  from_h=$(commit_first_parent "$to_h")
fi

out=/tmp/out
{
  if [ -z "$from_h" ]; then
    echo "(no earlier revision to diff against — $to_h is a root commit)"
  else
    printf -- '---- diff %s..%s%s ----\n' "${from_h:0:12}" "${to_h:0:12}" "${path:+ ($path)}"
    diff_revs "$from_h" "$to_h" "$path"
  fi
} > "$out"

caos put "$out" /cas/out
