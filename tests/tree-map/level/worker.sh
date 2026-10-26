#!/bin/bash
# tests/tree-map's per-directory worker: the stand-in for what a real caller
# (rgrep) puts in tree-map's `run` arg. tree-map runs it with `--in` a tree of
# one directory's files; for each file containing the `tag` arg it leaves a
# result file `<tag>:<name>`, and a file without it leaves nothing. An `--in`
# that is a single file gets the blob `file:<tag>`.
set -euo pipefail

caos get /cas/args/tag
tag=$(cat /cas/args/tag)
in=/cas/args/in
caos get -r "$in"

if [ ! -d "$in" ]; then
  printf 'file:%s\n' "$tag" > /tmp/out
  caos put /tmp/out /cas/out
  exit 0
fi

mkdir -p /tmp/out
for f in "$in"/*; do
  [ -f "$f" ] || continue
  if grep -qF -- "$tag" "$f"; then
    printf '%s:%s\n' "$tag" "$(basename "$f")" > "/tmp/out/$(basename "$f")"
  fi
done
caos put /tmp/out /cas/out
