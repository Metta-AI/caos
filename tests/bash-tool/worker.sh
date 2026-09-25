#!/bin/bash
# tests/bash-tool — a WORKER test: no client, no repo.
#
# Exercises the bounded bash tool (std/bash-tool, design/agent-harness.md): a
# command over a source tree with only the *declared* paths materialized.
# Asserts: a targeted read touches only its declared path and the result tree
# round-trips the source tree identically; an undeclared touch fails with EACCES
# and a structured `denied` retry hint; writes stage back correctly with
# untouched placeholder subtrees intact by hash; a failing command is a VALUE
# ({exit, stdout, stderr, tree}), never a run error; and the exec bit survives.
#
# THE TREE IS `in`. Every arg is loose under /cas/args -- `cmd`, `paths` -- and
# the tree the command runs over is `in`, which is the one name the interpreter
# binds on a tool run and therefore the only one a caller can redirect (`@in` in
# the tool's help). A tool reached at `caos-std/bash-tool` sits in a tree that is
# never the tree a caller means, so it has to be nameable.
#
# SIX STAGES: a run cannot be waited on, so each assertion is the `then` of the
# run it is about. Trees are compared by OID — a caos object hash IS a git
# object hash, and the fixture arrives as one.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --tool:@=/cas/args/tool --ws:@=/cas/args/ws "$@"; }

ws_oid() { caos hash /cas/args/ws; }

# The request the tool runs from: `in` (the tree) plus its parameters.
req() { # <cmd> [paths] -> a request hash
  # THE TREE IS `in`, beside `cmd` and `paths` -- the shape every tool with `@in`
  # has, and the reason it is `in` rather than a name of its own: `in` is what the
  # interpreter binds on a tool run, so naming it is what lets a caller redirect
  # the tree (llm-step's RESERVED_ARGS, and `@in` in this tool's help).
  #
  # `prepare-request` + `run-request-then` rather than `run-then`, because
  # `run-then` INVENTS an `--in` around the tree it is given -- which is how the
  # envelope `{tree, cmd, paths}` this used to build arrived at `in` in the first
  # place, and would now shadow the tree.
  local paths=()
  if [ $# -ge 2 ]; then paths=(--paths="$2"); fi
  caos prepare-request --base:hash="$(caos hash /cas/args/tool)" \
    --in:@=/cas/args/ws --cmd="$1" "${paths[@]}" \
    || fail "forming the request"
}

case "$stage" in

start)
  echo "== targeted read: declared path only; source_tree round-trips by hash ==" >&2
  caos run-request-then "$(req 'cat a/one.txt' 'a/one.txt')" \
    --then:hash="$(next read)"
  ;;

read)
  R=/cas/args/result; caos get -r "$R" || fail "reading the result"
  [ "$(cat "$R/exit")" = "0" ] || fail "read: exit $(cat "$R/exit")"
  [ "$(cat "$R/stdout")" = "one" ] || fail "read: stdout $(cat "$R/stdout")"
  [ ! -e "$R/failed" ] || fail "a clean run left a failed marker"
  [ -s "$R/out" ] || fail "the result carries no rendered out"
  [ "$(caos hash "$R/prop")" = "$(ws_oid)" ] || fail "read-only run changed the tree"
  echo "  ok: read its file; tree unchanged (identical hash)" >&2

  echo "== undeclared touch: EACCES + structured retry hint ==" >&2
  caos run-request-then "$(req 'cat a/b/two.txt' 'top.txt')" \
    --then:hash="$(next denied)"
  ;;

denied)
  R=/cas/args/result; caos get -r "$R" || fail "reading the result"
  [ "$(cat "$R/exit")" != "0" ] || fail "undeclared read did not fail"
  grep -qi "permission denied" "$R/stderr" || fail "no EACCES in stderr"
  [ -f "$R/denied" ] || fail "no denied hint in the result"
  grep -q "a/b/two.txt" "$R/denied" || fail "hint misses the path"
  echo "  ok: EACCES surfaced, denied names a/b/two.txt" >&2

  echo "== writes staged back; untouched placeholder subtree intact by hash ==" >&2
  caos run-request-then "$(req 'echo hi > new.txt && echo edited >> a/one.txt' 'a/one.txt')" \
    --then:hash="$(next write)"
  ;;

write)
  R=/cas/args/result; caos get -r "$R" || fail "reading the result"
  [ "$(cat "$R/exit")" = "0" ] || fail "write: exit $(cat "$R/exit")"
  [ "$(cat "$R/prop/new.txt")" = "hi" ] || fail "created file missing/wrong"
  [ "$(cat "$R/prop/a/one.txt")" = "$(printf 'one\nedited')" ] || fail "edit not staged"
  caos get -r /cas/args/ws || fail "reading the fixture"
  [ "$(caos hash "$R/prop/a/b")" = "$(caos hash /cas/args/ws/a/b)" ] \
    || fail "untouched subtree a/b did not round-trip by hash"
  [ "$(cat "$R/prop/top.txt")" = "top" ] || fail "untouched top.txt lost"
  echo "  ok: new.txt + edit staged, a/b round-tripped" >&2

  echo "== a failing command is a value, not a run error ==" >&2
  caos run-request-then "$(req 'echo oops >&2; exit 7')" \
    --then:hash="$(next failed)"
  ;;

failed)
  R=/cas/args/result; caos get -r "$R" || fail "reading the result"
  [ "$(cat "$R/exit")" = "7" ] || fail "exit code not surfaced: $(cat "$R/exit")"
  grep -q "oops" "$R/stderr" || fail "stderr not captured"
  # `failed` PRESENT is what makes the call an error tool_result. A marker, not
  # a banner in `out`: command output says FAILED in passing all the time.
  [ -e "$R/failed" ] || fail "a non-zero exit left no failed marker"
  [ "$(caos hash "$R/prop")" = "$(ws_oid)" ] || fail "failed run mangled the tree"
  echo "  ok: exit 7 + stderr returned as a value" >&2

  echo "== the executable bit round-trips (declared, loaded copy) ==" >&2
  caos run-request-then "$(req './run.sh' 'run.sh')" \
    --then:hash="$(next execbit)"
  ;;

execbit)
  R=/cas/args/result; caos get -r "$R" || fail "reading the result"
  [ "$(cat "$R/exit")" = "0" ] || fail "exec run: exit $(cat "$R/exit")"
  [ "$(cat "$R/stdout")" = "hi" ] || fail "declared file was not executable"
  [ "$(caos hash "$R/prop")" = "$(ws_oid)" ] || fail "exec bit lost round-tripping"
  echo "  ok: ./run.sh ran and the 100755 mode round-tripped" >&2

  printf 'bash-tool: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
