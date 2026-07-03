#!/usr/bin/env bash
# Regenerate the committed benchmark fixture at tests/fold-bench/tree.
#
# Shape: depth 4; every internal dir holds SUBDIRS subdirs + FILES files; the
# deepest dirs hold FILES files only. Every file's content is its own path, so
# every subtree hash is unique — no two fold requests alias, which keeps the
# benchmark an honest worst case for memoization (a real source tree, where
# most subtrees differ, behaves the same way).
#
# 40 dirs + 120 files = 160 nodes; `fold --post=file-count` must report 120.
set -euo pipefail
cd "$(dirname "$0")"
DEPTH=3 SUBDIRS=3 FILES=3

gen() { # <dir> <depth>
  local dir=$1 depth=$2 i
  mkdir -p "$dir"
  for i in $(seq 1 $FILES); do
    printf '%s\n' "$dir/f$i.txt" >"$dir/f$i.txt"
  done
  if [ "$depth" -lt "$DEPTH" ]; then
    for i in $(seq 1 $SUBDIRS); do
      gen "$dir/d$i" $((depth + 1))
    done
  fi
}

rm -rf tree
gen tree 0
echo "generated $(find tree -type f | wc -l | tr -d ' ') files in $(find tree -type d | wc -l | tr -d ' ') dirs" >&2
