#!/usr/bin/env bash
# Regenerate the committed fold-wasm fixtures.
#
# wide/: depth 3, 2 subdirs + 2 files per dir (+ one symlink-to-file at the
# root — a leaf blob for both fold implementations, so file-count totals 31). Unique file contents so no two
# subtrees alias (see fold-bench/gen-tree.sh).
#
# deep/: a 24-level chain with one file per level (24 files) — deeper than any
# worker pool is wide, so it deadlocks a single-slot warm pool and proves the
# isolate host holds arbitrarily many suspended fold frames.
set -euo pipefail
cd "$(dirname "$0")"

gen() { # <dir> <depth> <maxdepth>
  local dir=$1 depth=$2 max=$3 i
  mkdir -p "$dir"
  for i in 1 2; do printf '%s\n' "$dir/f$i.txt" >"$dir/f$i.txt"; done
  if [ "$depth" -lt "$max" ]; then
    for i in 1 2; do gen "$dir/d$i" $((depth + 1)) "$max"; done
  fi
}

rm -rf wide deep
gen wide 0 3
ln -s f1.txt wide/link-to-file

d=deep
for i in $(seq 1 24); do
  mkdir -p "$d"
  printf 'level %s\n' "$i" >"$d/f.txt"
  d="$d/d"
done

echo "wide: $(find wide -type f | wc -l | tr -d ' ') files; deep: $(find deep -type f | wc -l | tr -d ' ') files" >&2
