# ---- log ------------------------------------------------------------------
githist_init

start=HEAD
if caos get /cas/args/rev >/dev/null 2>&1; then start=$(cat /cas/args/rev); fi
path=""
if caos get /cas/args/path >/dev/null 2>&1; then path=$(cat /cas/args/path); fi
count=20
if caos get /cas/args/count >/dev/null 2>&1; then count=$(cat /cas/args/count); fi
case "$count" in ''|*[!0-9]*) count=20 ;; esac

h=$(resolve_rev "$start")

out=/tmp/out
: > "$out"
shown=0
while [ -n "$h" ] && [ "$shown" -lt "$count" ]; do
  include=1
  if [ -n "$path" ]; then
    parent=$(commit_first_parent "$h")
    cur=$(path_oid "$h" "$path")
    prev=""
    if [ -n "$parent" ]; then prev=$(path_oid "$parent" "$path"); fi
    if [ "$cur" = "$prev" ]; then include=0; fi
  fi
  if [ "$include" -eq 1 ]; then
    IFS=$'\t' read -r name ts <<< "$(commit_author_date "$h")"
    printf '%s  %s  %s  %s\n' "${h:0:12}" "$(fmt_date "$ts")" "$name" "$(commit_subject "$h")" >> "$out"
    shown=$((shown + 1))
  fi
  h=$(commit_first_parent "$h")
done
if [ "$shown" -eq 0 ]; then
  if [ -n "$path" ]; then echo "(no commits touched $path)" >> "$out"; else echo "(no history)" >> "$out"; fi
fi
caos put "$out" /cas/out
