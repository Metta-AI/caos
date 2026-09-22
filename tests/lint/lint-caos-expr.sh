#!/bin/bash
# lint-caos-expr.sh <root> — every `.caos-expr` directive is ONE line.
#
# `eval` splits on `text.lines()`, so a trailing `\` is not a continuation but
# a TOKEN: it survives into the argument parser and comes back as
# `argument must look like --name=value, got: \`, which reads as a malformed
# argument on a line whose arguments are all well formed.
#
# What it cost the one time it happened: a client repo's root expression was
# written across four lines, copied from design/flake-inputs.md, which wrapped
# that same expression for the page while the prose above it said "is one
# line". Every resolution of its mounted `caos-std/` failed, so `caos mcp warm`
# retried 120 times over 2m17s and gave up, and a cloud session recorded its
# prompt and then never took a turn -- `UserPromptSubmit` cannot form a request
# without resolving the step. Nothing in that trail named a backslash.
#
# `eval` refuses it by name now. This is the earlier of the two nets: it fires
# in the suite, before anything evaluates.
#
# (It used to also check that a checked-in client repo's expression pinned the
# same commit its flake.lock locked. That went with the repo: the template now
# lives in Metta-AI/caos-session, where it is the thing people fork rather than
# a copy of it, and std/flake-input-loader makes the same comparison there at
# evaluation time.)
#
# Tools: bash, coreutils, findutils, gnugrep -- what `std/bash` carries. There
# is no sed and no awk here (AGENTS.md, "Shell").
set -euo pipefail

root="${1:?usage: lint-caos-expr.sh <root>}"

status=0
checked=0

while IFS= read -r expr; do
    checked=$((checked + 1))
    n=0
    # `|| [ -n "$line" ]` because a file whose LAST line has no trailing
    # newline would otherwise lose that line entirely -- and the last line is
    # exactly where a one-expression file's directive sits.
    while IFS= read -r line || [ -n "$line" ]; do
        n=$((n + 1))
        case "$(printf '%s' "$line" | tr -d '[:space:]')" in ''|'#'*) continue ;; esac
        case "$line" in
            *\\) echo "FAIL: $expr line $n ends in a backslash; a .caos-expr" >&2
                 echo "      directive is ONE line and has no continuations" >&2
                 status=1 ;;
        esac
    done < "$expr"
done < <(find "$root" -name .caos-expr -not -path '*/.git/*')

# VACUOUS IS A FAILURE. This walks a glob, so a tree that was never mounted
# produces a clean pass that checked nothing -- the exact shape the other two
# lints assert their mounts against in worker.sh. Any real caos tree has
# `.caos-expr` files under std/.
if [ "$checked" = 0 ]; then
    echo "FAIL: no .caos-expr found under $root — the tree was not mounted" >&2
    exit 1
fi

if [ "$status" = 0 ]; then
    printf 'every .caos-expr directive is one line (%s file(s) checked)\n' "$checked"
fi
exit "$status"
