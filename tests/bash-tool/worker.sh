#!/bin/bash
# tests/bash-tool — a WORKER test: no client, no repo.
#
# Exercises the bounded bash tool (std/bash-tool, design/agent-harness.md): a
# command over a workspace tree with only the *declared* paths materialized.
# Asserts: a targeted read touches only its declared path and the result tree
# round-trips the workspace identically; an undeclared touch fails with EACCES
# and a structured `denied` retry hint; writes stage back correctly with
# untouched placeholder subtrees intact by hash; a failing command is a VALUE
# ({exit, stdout, stderr, tree}), never a run error; and the exec bit survives.
#
# THE REQUEST IS A BUNDLE, which is the worker's OWN preferred shape rather than
# an accommodation: std/bash-tool reads `{tree, cmd, paths}` out of `--in` when
# it has one and falls back to three loose args only when it does not — "how a
# run-then sub-run passes it", says the comment there. This IS a run-then
# sub-run, so it takes the first branch.
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

# `{tree, cmd, paths}`, published so it can be a run's subject.
req() { # <cmd> [paths] -> a /cas path
  rm -rf /tmp/req && mkdir -p /tmp/req
  caos get -r /cas/args/ws || fail "reading the fixture"
  cp -rL /cas/args/ws /tmp/req/tree && chmod -R u+w /tmp/req/tree
  printf '%s' "$1" > /tmp/req/cmd
  if [ $# -ge 2 ]; then printf '%s\n' "$2" > /tmp/req/paths; fi
  caos put /tmp/req /cas/req || fail "publishing the request"
  echo /cas/req
}

case "$stage" in

start)
  echo "== targeted read: declared path only; workspace round-trips by hash ==" >&2
  caos run-then "$(req 'cat a/one.txt' 'a/one.txt')" \
    --run:hash="$(caos hash /cas/args/tool)" --then:hash="$(next read)"
  ;;

read)
  R=/cas/args/result; caos get -r "$R" || fail "reading the result"
  [ "$(cat "$R/exit")" = "0" ] || fail "read: exit $(cat "$R/exit")"
  [ "$(cat "$R/stdout")" = "one" ] || fail "read: stdout $(cat "$R/stdout")"
  [ "$(caos hash "$R/tree")" = "$(ws_oid)" ] || fail "read-only run changed the tree"
  echo "  ok: read its file; tree unchanged (identical hash)" >&2

  echo "== undeclared touch: EACCES + structured retry hint ==" >&2
  caos run-then "$(req 'cat a/b/two.txt' 'top.txt')" \
    --run:hash="$(caos hash /cas/args/tool)" --then:hash="$(next denied)"
  ;;

denied)
  R=/cas/args/result; caos get -r "$R" || fail "reading the result"
  [ "$(cat "$R/exit")" != "0" ] || fail "undeclared read did not fail"
  grep -qi "permission denied" "$R/stderr" || fail "no EACCES in stderr"
  [ -f "$R/denied" ] || fail "no denied hint in the result"
  grep -q "a/b/two.txt" "$R/denied" || fail "hint misses the path"
  echo "  ok: EACCES surfaced, denied names a/b/two.txt" >&2

  echo "== writes staged back; untouched placeholder subtree intact by hash ==" >&2
  caos run-then "$(req 'echo hi > new.txt && echo edited >> a/one.txt' 'a/one.txt')" \
    --run:hash="$(caos hash /cas/args/tool)" --then:hash="$(next write)"
  ;;

write)
  R=/cas/args/result; caos get -r "$R" || fail "reading the result"
  [ "$(cat "$R/exit")" = "0" ] || fail "write: exit $(cat "$R/exit")"
  [ "$(cat "$R/tree/new.txt")" = "hi" ] || fail "created file missing/wrong"
  [ "$(cat "$R/tree/a/one.txt")" = "$(printf 'one\nedited')" ] || fail "edit not staged"
  caos get -r /cas/args/ws || fail "reading the fixture"
  [ "$(caos hash "$R/tree/a/b")" = "$(caos hash /cas/args/ws/a/b)" ] \
    || fail "untouched subtree a/b did not round-trip by hash"
  [ "$(cat "$R/tree/top.txt")" = "top" ] || fail "untouched top.txt lost"
  echo "  ok: new.txt + edit staged, a/b round-tripped" >&2

  echo "== a failing command is a value, not a run error ==" >&2
  caos run-then "$(req 'echo oops >&2; exit 7')" \
    --run:hash="$(caos hash /cas/args/tool)" --then:hash="$(next failed)"
  ;;

failed)
  R=/cas/args/result; caos get -r "$R" || fail "reading the result"
  [ "$(cat "$R/exit")" = "7" ] || fail "exit code not surfaced: $(cat "$R/exit")"
  grep -q "oops" "$R/stderr" || fail "stderr not captured"
  [ "$(caos hash "$R/tree")" = "$(ws_oid)" ] || fail "failed run mangled the tree"
  echo "  ok: exit 7 + stderr returned as a value" >&2

  echo "== the executable bit round-trips (declared, loaded copy) ==" >&2
  caos run-then "$(req './run.sh' 'run.sh')" \
    --run:hash="$(caos hash /cas/args/tool)" --then:hash="$(next execbit)"
  ;;

execbit)
  R=/cas/args/result; caos get -r "$R" || fail "reading the result"
  [ "$(cat "$R/exit")" = "0" ] || fail "exec run: exit $(cat "$R/exit")"
  [ "$(cat "$R/stdout")" = "hi" ] || fail "declared file was not executable"
  [ "$(caos hash "$R/tree")" = "$(ws_oid)" ] || fail "exec bit lost round-tripping"
  echo "  ok: ./run.sh ran and the 100755 mode round-tripped" >&2

  printf 'bash-tool: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
