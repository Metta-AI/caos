#!/bin/bash
# Read a caos-client repo's PIN: which caos it declares, and where in its
# evaluated tree caos' `std/` is mounted.
#
#   eval "$(bash caos-pin.sh /home/user/my-repo)"
#   # sets: caos_pin_base, caos_pin_std_path, caos_pin_rev, caos_pin_repo
#   # prints nothing and exits 1 when the checkout does not pin caos
#
# WHY THIS EXISTS AS ITS OWN FILE: two callers need the same answer, at
# different times. `setup.sh` reads it once, before the environment is
# snapshotted, to install the right client; `session-start.sh` reads it again
# every session, because the snapshot freezes whatever setup installed and a
# repo whose pin has since moved would otherwise keep getting the old client and
# the old tools -- the half-update that presents as "my fix did not take". One
# implementation, fetched by both, rather than the same jq written twice.
#
# WHICH PIN IT READS, and why that is not arbitrary: `flake.lock`. The
# `.caos-expr` carries the same locator, and `std/flake-input-loader` REFUSES to
# evaluate a tree whose expression and lockfile disagree (naming both revisions)
# -- so drift is loud wherever it is read from, and the lockfile is the easier
# parse: the rev appears once, under `locked`, where the expression carries the
# locator twice on purpose (`--base` for the loader's image, `--input-tree` for
# the tree it splices).
#
# This is the SAME node walk `std/flake-input-loader` does (`locked_input` in
# its src/main.rs): `nodes.<root>.inputs.<name>` maps the input name to a node
# key, and only that node's `locked` section is authoritative. Guessing the key
# from the name works until someone renames an input.
set -uo pipefail

dir="${1:-}"
input="${CAOS_PIN_INPUT:-caos}"
RAW="https://raw.githubusercontent.com"

say() { printf 'caos-pin: %s\n' "$*" >&2; }

if [ -z "$dir" ] || [ ! -d "$dir" ]; then
    say "no such directory: ${dir:-<none given>}"
    exit 1
fi
lock="$dir/flake.lock"
if [ ! -r "$lock" ]; then
    # Not an error worth shouting about: most repositories are not caos-client
    # repos, and the caller falls back to its own --base for those.
    say "$dir has no flake.lock; not a caos-client repo"
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    say "jq is needed to read flake.lock and is not on PATH"
    exit 1
fi

# The locked node for `--input`, as SHELL ASSIGNMENTS -- one per line, `@sh`
# quoted -- rather than one delimited record.
#
# NOT `@tsv` read into positional variables, and the reason is a bug that
# already bit here: TAB is IFS *whitespace* in bash, so a run of them collapses
# however `IFS` is set, and a `github` node (which has no `url`) shifted every
# later field one to the left -- `rev` came back EMPTY and the script reported
# "not pinned to a commit" about a lockfile pinning it perfectly well. An empty
# field cannot be expressed in a whitespace-delimited record; `@sh` has no such
# hole.
node="$(jq -r --arg want "$input" '
    (.root // "root") as $root
    | (.nodes[$root].inputs[$want] // empty) as $key
    | if ($key | type) != "string" then empty
      else (.nodes[$key].locked // empty)
         | "ty=\((.type//"")|@sh) owner=\((.owner//"")|@sh)",
           "repo=\((.repo//"")|@sh) url=\((.url//"")|@sh) rev=\((.rev//"")|@sh)"
      end' "$lock" 2>/dev/null)"
if [ -z "$node" ]; then
    say "flake.lock does not lock an input called '$input'"
    say "  (a 'follows' input carries no lock of its own, and is not one either)"
    exit 1
fi
ty=""; owner=""; repo=""; url=""; rev=""
eval "$node"

if [ -z "$rev" ]; then
    say "the '$input' input is not pinned to a commit (no rev in flake.lock)"
    exit 1
fi

# Only the shapes that yield a GitHub raw base, because that is what install.sh
# takes. A `git+https://github.com/...` input locks as type `git` with the URL
# in `url`; a `github:` input locks as type `github` with owner/repo split out.
case "$ty" in
    github)
        [ -n "$owner" ] && [ -n "$repo" ] || { say "github input lacks owner/repo"; exit 1; }
        ;;
    git)
        # Peel the URL down to owner/repo. `git+` prefix, an optional `.git`
        # suffix and any query (`?ref=main`) all have to come off before the
        # last two path components mean anything.
        u="${url#git+}"; u="${u%%\?*}"; u="${u%.git}"; u="${u%/}"
        case "$u" in
            https://github.com/*)
                u="${u#https://github.com/}"
                owner="${u%%/*}"; repo="${u#*/}"
                ;;
            *)
                say "the '$input' input is at $url, which is not github.com --"
                say "  install.sh downloads its client from a GitHub release, so"
                say "  a non-GitHub pin cannot be turned into a --base."
                exit 1
                ;;
        esac
        [ -n "$owner" ] && [ -n "$repo" ] && [ "$owner" != "$repo" ] \
            || { say "could not read owner/repo out of $url"; exit 1; }
        ;;
    *)
        say "the '$input' input has type '$ty', which carries no URL to build a --base from"
        exit 1
        ;;
esac

# WHERE caos' std LANDS in this repo's evaluated tree: the `--output-path` of
# the root expression's flake-input-loader line. Read from `.caos-expr` rather
# than assumed, because it is the consumer's choice and the one place that
# states it -- `--llm-step:@=<here>/llm-step` has to name the same directory or
# the tool server resolves nothing.
#
# The loader's own rule for comments is followed (full-line `#` only), so a
# commented-out example line does not win over the real one.
std_path=""
expr_file="$dir/.caos-expr"
if [ -r "$expr_file" ]; then
    # `|| [ -n "$line" ]` because a file whose LAST line has no trailing newline
    # would otherwise lose that line entirely -- `read` sets it and then returns
    # non-zero at EOF, so the loop exits before the body runs. The last line is
    # exactly where `--output-path` sits in a one-expression file, so without
    # this the answer is "names no --output-path" about an expression that names
    # one on the line you are looking at.
    while IFS= read -r line || [ -n "$line" ]; do
        case "$(printf '%s' "$line" | tr -d '[:space:]')" in ''|'#'*) continue ;; esac
        for token in $line; do
            case "$token" in
                --output-path=*)
                    value="${token#--output-path=}"
                    if [ -n "$std_path" ] && [ "$std_path" != "$value" ]; then
                        say "$expr_file names two different --output-path values"
                        say "  ($std_path and $value); this cannot tell which mounts caos."
                        exit 1
                    fi
                    std_path="$value"
                    ;;
            esac
        done
    done < "$expr_file"
fi
if [ -z "$std_path" ]; then
    say "$expr_file names no --output-path, so nothing says where caos' std is"
    say "  mounted. A caos-client repo's root expression loads the pinned input:"
    say "  run --base:@@=...&dir=std/flake-input-loader --in:@=. --expr=\$CAOS_EXPR \\"
    say "      --input=$input --input-tree:@@=...&dir=std --output-path=caos-std"
    exit 1
fi

# `declare`-free on purpose: the caller `eval`s this, and these are plain
# assignments so it works in any shell and under `set -u`.
printf 'caos_pin_base=%s\n' "$RAW/$owner/$repo/$rev"
printf 'caos_pin_std_path=%s\n' "$std_path"
printf 'caos_pin_repo=%s\n' "$owner/$repo"
printf 'caos_pin_rev=%s\n' "$rev"
