#!/bin/bash
# tests/deep-deps — a WORKER test: no client, no repo.
#
# deep-deps (design/caos-expr.md) restructures a tree so that every directory
# carrying a `DEPS` file gets its dependencies, recursively deepened, mounted
# inside it under `DEEP-DEPS/`. A `DEPS` line is `<path> <name>`: the path is
# relative to the DEPS file's OWN directory (so `../..` reaches parents), and
# the name is the mount. There is no special `packages` root — it is a
# whole-tree transform. We check the deepened shape, DAG sharing, incremental
# recompute, FILE deps, cycle detection, and its use THROUGH evaluation.
#
# THE FIXTURE IS BUILT AND PUBLISHED, not committed: a worker has no git, and
# `caos put` is how it makes a tree nameable. Each stage builds exactly the
# variant its own run needs, from the same function — so every input is a stated
# function of the fixture rather than of the edits before it.
#
# EIGHT STAGES: no run can be waited on, so each assertion is the `then` of the
# deepen it is about.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --deep-deps:@=/cas/args/deep-deps "$@"; }

# The fixture: `app` depends on two libs by relative path; `lib/foo` depends on
# its sibling `lib/bar`. So `bar` is reached three ways (app's dep, foo's dep,
# and structurally under lib) — one shared node. `$1` selects a variant.
fixture() { # <variant> -> a /cas path
  local r=/tmp/tree
  rm -rf "$r"; mkdir -p "$r/app" "$r/lib/foo" "$r/lib/bar"
  echo "app main" > "$r/app/main.txt"
  printf '../lib/foo foo\n../lib/bar bar\n' > "$r/app/DEPS"
  echo "foo lib" > "$r/lib/foo/foo.txt"
  printf '../bar shared\n' > "$r/lib/foo/DEPS"
  echo "bar lib" > "$r/lib/bar/bar.txt"
  case "$1" in
    other)   mkdir -p "$r/other"; echo x > "$r/other/x.txt" ;;
    bar-v2)  echo "bar lib v2" > "$r/lib/bar/bar.txt" ;;
    file-dep)
      echo "root manifest" > "$r/Manifest.toml"
      printf '../lib/foo foo\n../lib/bar bar\n../Manifest.toml Manifest.toml\n' > "$r/app/DEPS" ;;
    cycle)   printf '../../app loop\n' > "$r/lib/bar/DEPS" ;;
    evtree)
      # A `.caos-expr` at the tree root deepens the whole tree; evaluating a
      # path then descends into the deepened result. The worker is named by a
      # path INSIDE the tree (there is no ambient `/std/<name>`), so the entry
      # this test declared is copied in — what a DEPS mount would have produced.
      caos get -r /cas/args/deep-deps || fail "reading the deep-deps entry"
      cp -rL /cas/args/deep-deps "$r/deep-deps" && chmod -R u+w "$r/deep-deps"
      printf 'run --base:@=deep-deps --in:@=.\n' > "$r/.caos-expr" ;;
  esac
  caos put "$r" "/cas/fixture-$1" || fail "publishing the fixture"
  echo "/cas/fixture-$1"
}

deepen() { # <variant> <next-stage> [extra curry args...]
  local v=$1 s=$2; shift 2
  caos run-then "$(fixture "$v")" --run:hash="$(caos hash /cas/args/deep-deps)" \
    --then:hash="$(next "$s" "$@")"
}

# Keep a result for a later comparison: published as a blob holding its oid.
keep() { printf '%s\n' "$(caos hash /cas/args/result)" > /tmp/oid; caos put /tmp/oid /cas/oid; echo /cas/oid; }

case "$stage" in

start)
  echo "== deepened shape: DEPS replaced by DEEP-DEPS, paths resolved ==" >&2
  deepen plain shape
  ;;

shape)
  A=/cas/args/result; caos get -r "$A" || fail "reading the deepened tree"
  [ -e "$A/app/DEEP-DEPS/foo/foo.txt" ]                  || fail "app should mount foo"
  [ -e "$A/app/DEEP-DEPS/bar/bar.txt" ]                  || fail "app should mount bar"
  [ -e "$A/app/DEEP-DEPS/foo/DEEP-DEPS/shared/bar.txt" ] || fail "foo should mount bar as shared"
  [ -e "$A/app/main.txt" ]                               || fail "app's own files should survive"
  [ -e "$A/app/DEPS" ]                                   && fail "DEPS should be dropped from nodes"
  [ -e "$A/lib/foo/DEEP-DEPS/shared/bar.txt" ]           || fail "lib/foo should also be deepened"
  echo "  ok: relative <path> resolved, <name> mounted, DEPS dropped" >&2

  echo "== DAG sharing: bar is one identical node everywhere it appears ==" >&2
  diff -r "$A/app/DEEP-DEPS/bar" "$A/lib/bar" >/dev/null \
    || fail "app's bar and lib/bar should be identical"
  diff -r "$A/app/DEEP-DEPS/foo" "$A/lib/foo" >/dev/null \
    || fail "app's foo and lib/foo should be identical"
  diff -r "$A/app/DEEP-DEPS/foo/DEEP-DEPS/shared" "$A/lib/bar" >/dev/null \
    || fail "foo's shared dep should be the same node as lib/bar"
  echo "  ok: shared subgraphs are byte-identical" >&2

  echo "== editing an unrelated package leaves app,lib untouched ==" >&2
  deepen other unrelated --a="$(caos hash "$A/app")" --alib="$(caos hash "$A/lib")"
  ;;

unrelated)
  B=/cas/args/result; caos get -r "$B" || fail "reading the deepened tree"
  caos get /cas/args/a; caos get /cas/args/alib
  [ "$(caos hash "$B/app")" = "$(cat /cas/args/a)" ] || fail "app changed after an unrelated edit"
  [ "$(caos hash "$B/lib")" = "$(cat /cas/args/alib)" ] || fail "lib changed after an unrelated edit"
  [ -e "$B/other/x.txt" ] || fail "the new dir is missing from the output"
  echo "  ok: app,lib byte-identical; other added" >&2

  echo "== editing shared leaf bar recomputes everything that reaches it ==" >&2
  deepen bar-v2 recompute --a:@=/cas/args/a
  ;;

recompute)
  C=/cas/args/result; caos get -r "$C" || fail "reading the deepened tree"
  caos get /cas/args/a
  [ "$(caos hash "$C/app")" = "$(cat /cas/args/a)" ] && fail "app should change when bar changes"
  [ "$(cat "$C/app/DEEP-DEPS/bar/bar.txt")" = "bar lib v2" ] || fail "app's bar not updated"
  [ "$(cat "$C/app/DEEP-DEPS/foo/DEEP-DEPS/shared/bar.txt")" = "bar lib v2" ] \
    || fail "foo's shared bar not updated"
  echo "  ok: every node reaching bar recomputed" >&2

  echo "== a FILE dependency is mounted, not walked ==" >&2
  deepen file-dep filedep
  ;;

filedep)
  E=/cas/args/result; caos get -r "$E" || fail "reading the deepened tree"
  [ -f "$E/app/DEEP-DEPS/Manifest.toml" ] || fail "the file dep is not mounted as a file"
  [ "$(cat "$E/app/DEEP-DEPS/Manifest.toml")" = "root manifest" ] \
    || fail "the mounted file's content is wrong"
  # Its directory siblings still deepen, so one file dep does not disturb the rest.
  [ -e "$E/app/DEEP-DEPS/foo/DEEP-DEPS/shared/bar.txt" ] \
    || fail "a directory dep stopped deepening once a file dep was declared"
  echo "  ok: mounted verbatim, alongside deepened directory deps" >&2

  echo "== a dependency cycle is detected (by the worker) ==" >&2
  # Close a loop: lib/bar -> app, so app -> bar -> app. The deepen is ONE pass
  # in one worker, so nothing else can catch this: no server round trip, so no
  # run-cycle detection to fall back on. The worker tracks the chain itself and
  # must NAME it. `--catch` because the failure IS the assertion.
  caos run-then "$(fixture cycle)" --run:hash="$(caos hash /cas/args/deep-deps)" \
    --then:hash="$(next cycle)" --catch
  ;;

cycle)
  [ -e /cas/args/error ] || fail "expected the cyclic graph to fail, but the run succeeded"
  caos get /cas/args/error
  grep -q "dependency cycle" /cas/args/error \
    || fail "no cycle reported; got: $(cat /cas/args/error)"
  echo "  ok: run failed, naming the cycle" >&2

  echo "== through evaluation: a top-level .caos-expr invokes deep-deps ==" >&2
  # `eval-path-then` is the worker's way to have an expression walked — the
  # server does the same walk a client `eval-path` does (tests/eval-then holds
  # that claim), so this covers the same ground without a client.
  caos eval-path-then "$(fixture evtree)" --eval=app/DEEP-DEPS/foo \
    --then:hash="$(next evaluated)"
  ;;

evaluated)
  F=/cas/args/result; caos get -r "$F" || fail "reading the evaluated node"
  [ -e "$F/foo.txt" ] || fail "deepened foo missing foo.txt"
  [ -e "$F/DEEP-DEPS/shared/bar.txt" ] || fail "deepened foo missing its shared dep"
  echo "  ok: evaluation deepened the tree and dug into a package's node" >&2

  printf 'deep-deps: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
