#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (dev/run-test/run-test.sh).
#
# Exercises the bounded bash tool (worker-bash-tool, design/agent-harness.md):
# a command over a workspace tree with only the *declared* paths materialized.
# Asserts: a targeted read touches only its declared path and the result tree
# round-trips the workspace identically; an undeclared touch fails with EACCES
# and a structured `denied` retry hint; writes stage back correctly with
# untouched placeholder subtrees intact by hash; and a failing command is a
# VALUE ({exit, stdout, stderr, tree}), never a run error.
#
# The tool is DEEP-DEPS/bash-tool — a std source entry (std/bash-tool) built on
# resolution via rustc and curried onto the runner pool (design/caos-expr.md,
# Phase 3), no image of its own.
#
# A STAGED TEST (dev/run-test/run-test.sh's header): five runs, six stages, and
# no container ever parked on a job. Two adjustments follow from that:
#
#   THE WORKSPACE IS REBUILT EVERY STAGE, not built once. cli.sh runs once per
#   stage in a fresh client repo, so `make_ws` has to be a pure function of the
#   checked-in fixtures — which it is, and which is what makes `git rev-parse
#   HEAD:ws` name the same tree in every stage.
#
#   TREES ARE COMPARED BY OID IN THE CAS. The old `snap` helper checked a
#   result out, committed it and asked git for the tree hash; a result is
#   already an object, so `cas_hash` reads the oid directly. A caos object hash
#   IS a git object hash, so it still compares against `git rev-parse`.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

# The workspace under test: two levels, so undeclared subtrees stay
# placeholders. Deterministic, so every stage's `ws` is the same tree.
make_ws() {
  mkdir -p ws/a/b
  echo one > ws/a/one.txt
  echo two > ws/a/b/two.txt
  echo top > ws/top.txt
  # An executable file, to prove the exec bit round-trips both ways: as an
  # undeclared placeholder (resolved by hash) and as a declared, loaded copy.
  printf '#!/bin/sh\necho hi\n' > ws/run.sh
  chmod +x ws/run.sh
  git add -A && git -c user.email=test@caos -c user.name=caos commit -qm workspace
}

# The tool is DEEP-DEPS/bash-tool — a source std entry (std/bash-tool) built on
# resolution via rustc, curried onto the runner pool (design/caos-expr.md,
# Phase 3). No host binary is threaded in; the tree under test IS the source.
tool_img() {
  "$CAOS_CLI" curry --base:@=DEEP-DEPS/bash-tool
}

# THE REQUEST IS A BUNDLE, and that is the worker's OWN preferred shape rather
# than an accommodation: std/bash-tool reads `{tree, cmd, paths}` out of `--in`
# when it has one and falls back to three loose args only when it does not —
# "how a run-then sub-run passes it", says the comment there. A staged test IS a
# run-then sub-run, so it takes the first branch. Passing the workspace itself
# as the subject puts a tree where the worker expects a bundle, and it dies
# looking for `/cas/args/in/cmd`.
make_req() { # <cmd> [paths] -> builds ./req, the subject
  rm -rf req && mkdir -p req
  cp -r ws req/tree
  printf '%s' "$1" > req/cmd
  if [ $# -ge 2 ]; then printf '%s\n' "$2" > req/paths; fi
  git add -A && git -c user.email=test@caos -c user.name=caos commit -qm req
}

make_ws
ws_hash=$(git rev-parse HEAD:ws)

case "$STAGE" in

start)
  echo "== targeted read: declared path only; workspace round-trips by hash ==" >&2
  make_req 'cat a/one.txt' 'a/one.txt'
  stage_next read "$(tool_img)" req
  ;;

read)
  fetch_result
  [ "$(cat "$RESULT/exit")" = "0" ] || fail "read: exit $(cat "$RESULT/exit")"
  [ "$(cat "$RESULT/stdout")" = "one" ] || fail "read: stdout $(cat "$RESULT/stdout")"
  [ "$(cas_hash "$RESULT/tree")" = "$ws_hash" ] \
    || fail "read-only run changed the workspace tree"
  echo "  ok: read its file; tree unchanged (identical hash)" >&2

  echo "== undeclared touch: EACCES + structured retry hint ==" >&2
  make_req 'cat a/b/two.txt' 'top.txt'
  stage_next denied "$(tool_img)" req
  ;;

denied)
  fetch_result
  [ "$(cat "$RESULT/exit")" != "0" ] || fail "undeclared read did not fail"
  grep -qi "permission denied" "$RESULT/stderr" \
    || fail "no EACCES in stderr: $(cat "$RESULT/stderr")"
  [ -f "$RESULT/denied" ] || fail "no denied hint in the result"
  grep -q "a/b/two.txt" "$RESULT/denied" \
    || fail "hint misses the path: $(cat "$RESULT/denied")"
  echo "  ok: EACCES surfaced, denied names a/b/two.txt" >&2

  echo "== writes staged back; untouched placeholder subtree intact by hash ==" >&2
  make_req 'echo hi > new.txt && echo edited >> a/one.txt' 'a/one.txt'
  stage_next write "$(tool_img)" req
  ;;

write)
  fetch_result
  [ "$(cat "$RESULT/exit")" = "0" ] || fail "write: exit $(cat "$RESULT/exit")"
  [ "$(cat "$RESULT/tree/new.txt")" = "hi" ] || fail "created file missing/wrong"
  [ "$(cat "$RESULT/tree/a/one.txt")" = "$(printf 'one\nedited')" ] || fail "edit not staged"
  [ "$(cas_hash "$RESULT/tree/a/b")" = "$(git rev-parse HEAD:ws/a/b)" ] \
    || fail "untouched subtree a/b did not round-trip by hash"
  [ "$(cat "$RESULT/tree/top.txt")" = "top" ] || fail "untouched top.txt lost"
  echo "  ok: new.txt + edit staged, a/b round-tripped" >&2

  echo "== a failing command is a value, not a run error ==" >&2
  make_req 'echo oops >&2; exit 7'
  stage_next failed "$(tool_img)" req
  ;;

failed)
  fetch_result
  [ "$(cat "$RESULT/exit")" = "7" ] || fail "exit code not surfaced: $(cat "$RESULT/exit")"
  grep -q "oops" "$RESULT/stderr" || fail "stderr not captured"
  [ "$(cas_hash "$RESULT/tree")" = "$ws_hash" ] || fail "failed run mangled the tree"
  echo "  ok: exit 7 + stderr returned as a value" >&2

  echo "== the executable bit round-trips (declared, loaded copy) ==" >&2
  make_req './run.sh' 'run.sh'
  stage_next execbit "$(tool_img)" req
  ;;

execbit)
  fetch_result
  [ "$(cat "$RESULT/exit")" = "0" ] || fail "exec run: exit $(cat "$RESULT/exit")"
  [ "$(cat "$RESULT/stdout")" = "hi" ] \
    || fail "declared file was not executable: $(cat "$RESULT/stdout")"
  [ "$(cas_hash "$RESULT/tree")" = "$ws_hash" ] \
    || fail "exec bit lost round-tripping a declared/loaded file"
  echo "  ok: ./run.sh ran and the 100755 mode round-tripped" >&2

  echo "bash-tool: ALL PASS" >&2
  ;;

*) fail "unknown stage: $STAGE" ;;
esac
