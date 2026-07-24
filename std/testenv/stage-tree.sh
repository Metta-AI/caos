#!/usr/bin/env bash
# Assemble std/testenv's publishable tree: the flake, a lock derived from
# the main flake.lock (std/lib/derive-lock.sh — the pin cannot drift), and
# the script runner as ./worker (images/bash-worker.sh — one source of
# truth), which the flake places at /worker.
# Usage: stage-tree.sh <project root> <output dir> [bin store paths...]
set -euo pipefail
src=$1
out=$2

mkdir -p "$out"
cp "$src/std/testenv/flake.nix" "$out/flake.nix"
"$src/std/lib/derive-lock.sh" "$src/flake.lock" "$out/flake.lock" nixpkgs
install -m 755 "$src/images/bash-worker.sh" "$out/worker"
