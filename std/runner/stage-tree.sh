#!/usr/bin/env bash
# Assemble std/runner's publishable tree: the flake, a lock derived from the
# main flake.lock (std/lib/derive-lock.sh — the pin cannot drift), and the
# interpreter binary as ./worker (crates/worker-runner: fetch `worker1`,
# exec it), which the flake places at /worker.
# Usage: stage-tree.sh <project root> <output dir> <bin store paths...>
set -euo pipefail
src=$1
out=$2
shift 2

mkdir -p "$out"
cp "$src/std/runner/flake.nix" "$out/flake.nix"
"$src/std/lib/derive-lock.sh" "$src/flake.lock" "$out/flake.lock" nixpkgs
for p in "$@"; do
  [ -x "$p/bin/worker-runner" ] && install -m 755 "$p/bin/worker-runner" "$out/worker"
done
[ -e "$out/worker" ] || { echo "stage-tree: no worker-runner among: $*" >&2; exit 1; }
