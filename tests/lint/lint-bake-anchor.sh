#!/usr/bin/env bash
# tests/lint check: every crates.io dependency a source-built std tool
# declares must be present in the bake-anchor crate (crates/bake-anchor), so the
# std/cargo bake vendors+precompiles it (design/caos-expr.md, Phase 3). Source
# std tools live OUTSIDE the workspace and compile against that bake; a dep the
# anchor lacks would recompile from scratch or fail — the bake builds `--offline`
# against a vendored registry (std/cargo/bake.nix), so there is no crates.io to
# fall back to. That is the BUILD refusing to reach out, not the container being
# unable to: workers do have a network (AGENTS.md, "Workers").
#
# Names only: version/feature parity is caught by the build-time "did we reuse
# the bake?" guard (a mismatch recompiles, which that guard flags), which is far
# cheaper than reproducing cargo's feature unification here. Local/path deps
# (worker-common, llm-client) are NOT anchored — they are workspace crates / std
# entries resolved by splice/mount, never from crates.io.
set -euo pipefail
# THE TREE TO CHECK, as an argument — this script no longer lives at the root of
# the tree it reads. It is `tests/lint/`'s, and the tree is whatever that test
# assembled from its `DEPS` (or the repo root, run by hand from there).
root=${1:-.}
cd "$root"

# The crates.io dep NAMES in a Cargo.toml's [dependencies]: lines `name = …`
# that carry no `path` (a path dep is local). One name per line.
crates_io_deps() { # <Cargo.toml>
  sed -n '/^\[dependencies\]/,/^\[/p' "$1" \
    | grep -E '^[A-Za-z0-9_-]+[[:space:]]*=' \
    | grep -v 'path[[:space:]]*=' \
    | sed -E 's/^([A-Za-z0-9_-]+).*/\1/'
}

anchor=rust/crates/bake-anchor/Cargo.toml
[ -f "$anchor" ] || { echo "lint-bake-anchor: no $anchor" >&2; exit 1; }
anchored=$(crates_io_deps "$anchor" | sort -u)

fail=0
for toml in std/*/Cargo.toml; do
  [ -e "$toml" ] || continue
  while IFS= read -r dep; do
    [ -n "$dep" ] || continue
    if ! grep -qxF "$dep" <<<"$anchored"; then
      echo "lint-bake-anchor: $toml needs crates.io dep '$dep', but $anchor does not anchor it" >&2
      echo "  add '$dep' to $anchor so the std/cargo bake precompiles it" >&2
      fail=1
    fi
  done < <(crates_io_deps "$toml")
done

if [ "$fail" != 0 ]; then
  exit 1
fi
echo "lint-bake-anchor: every std tool's crates.io deps are anchored" >&2
