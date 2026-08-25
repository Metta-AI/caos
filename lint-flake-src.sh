#!/usr/bin/env bash
# Every file the workspace COMPILES FROM must survive the flake's `src` filter.
#
# THE GAP THIS EXISTS TO CLOSE. `run-tool test` compiles the tree with cargo
# over the real crates/ directory (caos-tools/test.sh stages the build tree as
# symlinks to Cargo.toml, Cargo.lock, rust-toolchain.toml and crates/), so
# every file on disk is there. `nix build` compiles a FILTERED copy. The filter
# is therefore the one input the suite never exercises, and anything it drops
# is invisible to a green run: crates/worker-llm-step/src/githist/*.sh went in
# under a passing suite and broke `nix build` on four include_str! calls,
# because crane's cleanCargoSource keeps *.rs, *.toml, Cargo.lock and
# .cargo/config and nothing else.
#
# So: find what the crates statically embed, and check the filter keeps it.
# KEEP_RULE below mirrors flake.nix's `src` filter. When you widen one, widen
# the other — they are two statements of one rule, and this file is the half
# that runs without nix. Drift is one-directional and cheap: a filter widened
# here but not there yields a red `nix build`; widened there but not here
# yields a red lint. Neither yields a stale green.
#
# Run it anywhere — no nix, no caos, no compile. `tests/std-lint` runs it in
# the suite; run it by hand from the repo root before committing.
set -euo pipefail

root=${1:-.}
cd "$root"

if [ ! -e flake.nix ] || [ ! -d rust/crates ]; then
  echo "lint-flake-src: $root is not the caos workspace root" >&2
  exit 2
fi

# THE FILTER IS ROOTED AT ./rust, so this reasons in the same frame: descend,
# and every path below — `crates/...`, `Cargo.lock` — means what flake.nix means
# by it. Everything cargo compiles lives under this one directory.
cd rust

# Mirrors flake.nix `src`. Returns 0 if the filter keeps this repo-relative path.
keep_rule() {
  local rel=$1
  case "$rel" in
    *.rs | *.toml | Cargo.lock | */Cargo.lock) return 0 ;;
    # Scripts a worker bakes into its binary are source, not data.
    crates/*.sh) return 0 ;;
  esac
  return 1
}

fails=0
seen=0

# `include!`, `include_str!`, `include_bytes!` with a LITERAL path. -o gives
# `file:line:include_str!("rel/path"` — no closing paren, which is what makes
# the quoted path the tail of the match.
pattern='include(_str|_bytes)?!\s*\(\s*"[^"]+"'

while IFS=: read -r file line match; do
  seen=$((seen + 1))
  # Strip through the opening quote, then the trailing one.
  rel=${match#*\"}
  rel=${rel%\"}

  target=$(realpath -m --relative-to=. "$(dirname "$file")/$rel")

  if [ ! -e "$target" ]; then
    echo "FAIL: $file:$line embeds '$rel' -> $target, which does not exist" >&2
    fails=$((fails + 1))
    continue
  fi

  if ! keep_rule "$target"; then
    echo "FAIL: $file:$line embeds $target, which the flake's src filter DROPS." >&2
    echo "      \`nix build\` will fail on it even though cargo and the suite pass." >&2
    echo "      Widen the filter in flake.nix AND keep_rule in this script." >&2
    fails=$((fails + 1))
  fi
done < <(grep -rnoE "$pattern" crates --include='*.rs' || true)

# An embed whose path is computed (concat!, env!, a macro) cannot be checked
# statically, and silently skipping it would be the same blind spot again. The
# raw count of embed sites must equal the count we resolved.
# -o prints one line per OCCURRENCE (-c would count lines and miss a second
# embed on the same line). The trailing `\(` is what keeps prose out of the
# count — githist.rs's own header says "include_str!" in a doc comment.
total=$(grep -roE 'include(_str|_bytes)?!\s*\(' crates --include='*.rs' | wc -l || true)

if [ "$total" -ne "$seen" ]; then
  echo "FAIL: $total include!/include_str!/include_bytes! sites under crates/, but only" >&2
  echo "      $seen have a literal path this lint can resolve. A computed path cannot be" >&2
  echo "      checked here — make it a literal, or teach this lint about it." >&2
  fails=$((fails + 1))
fi

if [ "$fails" -ne 0 ]; then
  echo "lint-flake-src: $fails problem(s)" >&2
  exit 1
fi

echo "lint-flake-src: $seen embedded file(s), all kept by the flake's src filter" >&2
