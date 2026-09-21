#!/bin/bash
# lint-client-repo-pin.sh <root> — a client repo's `.caos-expr` must pin the
# same caos commit its `flake.lock` locks.
#
# WHY THIS IS A LINT AND NOT LEFT TO THE LOADER. `std/flake-input-loader` makes
# exactly this comparison and refuses the evaluation when it fails, naming both
# revisions — so nothing can run against a drifted pin. But the tree that
# drifts here is a checked-in EXAMPLE nobody evaluates in the suite: it would
# sit broken in the repository until someone forked it, and the first thing
# they would see is their session refusing to start over two revisions they
# never typed. The loader protects the user; this protects the template.
#
# It is one `nix flake update caos` away at all times: that command rewrites
# flake.lock and touches nothing else, so the two `rev=` values in the
# expression are left naming the previous commit.
#
# SELF-SELECTING: only a directory whose flake.lock locks an input called
# `caos` is a client repo, so caos' own root (which has a flake.lock and no
# such input) is skipped without naming it here. `--caos` overrides the input
# name, matching `caos-pin.sh`'s CAOS_PIN_INPUT.
#
# Tools: bash, coreutils, findutils, gnugrep, jq — what `std/bash` carries.
# There is no sed and no awk here (AGENTS.md, "Shell").
set -euo pipefail

root="${1:?usage: lint-client-repo-pin.sh <root> [input-name]}"
input="${2:-caos}"

status=0
checked=0

while IFS= read -r lock; do
    dir="$(dirname "$lock")"
    # The same node walk `std/flake-input-loader` and `caos-pin.sh` do:
    # `nodes.<root>.inputs.<name>` maps the input NAME to a node KEY, which is
    # not the same string once an input has been renamed.
    rev="$(jq -r --arg want "$input" '
        (.root // "root") as $r
        | (.nodes[$r].inputs[$want] // empty) as $k
        | if ($k | type) != "string" then empty
          else (.nodes[$k].locked.rev // empty) end' "$lock" 2>/dev/null || true)"
    if [ -z "$rev" ]; then
        continue   # not a client repo: it locks no input by that name
    fi
    expr="$dir/.caos-expr"
    if [ ! -r "$expr" ]; then
        echo "FAIL: $dir locks $input at $rev but has no .caos-expr to mount it" >&2
        status=1
        continue
    fi
    checked=$((checked + 1))

    # Every `rev=` inside a `:@@=` locator, skipping full-line comments exactly
    # as the evaluator and the loader do — a commented-out example pins nothing.
    #
    # `|| true` on the grep: pipefail makes a pipeline fail on its LEFTMOST
    # failure, and grep exits 1 on no match, which here means "this line has no
    # locator" rather than an error (AGENTS.md, "Shell").
    found=0
    while IFS= read -r line; do
        case "$(printf '%s' "$line" | tr -d '[:space:]')" in ''|'#'*) continue ;; esac
        for token in $line; do
            case "$token" in
                *:@@=*rev=*) ;;
                *) continue ;;
            esac
            value="${token#*:@@=}"
            query="${value#*\?}"
            # `rev=` may sit anywhere in the query; take the one field.
            pinned=""
            IFS='&' read -ra fields <<< "$query"
            for field in "${fields[@]}"; do
                case "$field" in rev=*) pinned="${field#rev=}" ;; esac
            done
            [ -n "$pinned" ] || continue
            found=$((found + 1))
            if [ "$pinned" != "$rev" ]; then
                echo "FAIL: $expr pins $pinned but $dir/flake.lock locks $input at $rev" >&2
                echo "      (run \`nix flake update $input\` and copy the new rev into both" >&2
                echo "       locators, or the loader will refuse this tree)" >&2
                status=1
            fi
        done
    done < "$expr"

    if [ "$found" = 0 ]; then
        echo "FAIL: $expr names no ':@@=...rev=' locator, so nothing mounts $input" >&2
        status=1
    fi
done < <(find "$root" -name flake.lock -not -path '*/.git/*')

if [ "$status" = 0 ]; then
    printf 'client-repo pins agree with their lockfiles (%s repo(s) checked)\n' "$checked"
fi
exit "$status"
