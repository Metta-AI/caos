#!/usr/bin/env bash
# Assemble std/runner's publishable tree: the checked-in flake.nix and
# flake.lock (the lock is derived from the main flake.lock by std/refresh.sh
# and verified by tests/std-lint — the pin cannot drift unnoticed), plus the
# one thing not checked in: the interpreter binary as ./worker
# (crates/worker-runner: fetch `worker1`, exec it), which the flake places
# at /worker.
# Usage: stage-tree.sh <project root> <output dir> <bin store paths...>
set -euo pipefail
src=$1
out=$2
shift 2

mkdir -p "$out"
cp "$src/std/runner/flake.nix" "$out/flake.nix"
cp "$src/std/runner/flake.lock" "$out/flake.lock"
for p in "$@"; do
  [ -x "$p/bin/worker-runner" ] && install -m 755 "$p/bin/worker-runner" "$out/worker"
done
[ -e "$out/worker" ] || { echo "stage-tree: no worker-runner among: $*" >&2; exit 1; }
# $src may be a read-only store copy (caosd); writable so the next publish's
# rm -rf works.
chmod -R u+w "$out"
