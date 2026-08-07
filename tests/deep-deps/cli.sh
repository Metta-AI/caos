#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# deep-deps (design/caos-expr.md) restructures a tree so that every directory
# carrying a `DEPS` file gets its dependencies, recursively deepened, mounted
# inside it under `DEEP-DEPS/`. A `DEPS` line is `<path> <name>`: the path is
# relative to the DEPS file's OWN directory (so `../..` reaches parents), and
# the name is the mount. There is no special `packages` root — it is a
# whole-tree transform, published as the std entry /cas/std/deep-deps. That
# entry is itself a HAND-DEEPENED source entry (design/caos-expr.md, Phase 3):
# `{.caos-expr, worker, DEEP-DEPS/runner}`, the deepened form of a checked-in
# `{.caos-expr, DEPS, worker}` whose `.caos-expr` is `curry DEEP-DEPS/runner --
# --worker1:@=worker` — so deep-deps names its own runner base by a LOCAL
# deep-deps mount rather than ambient `/std/runner`, and resolving the entry
# evaluates that expression to the same curry node bootstrap published. We check
# the deepened shape, DAG sharing, incremental recompute, cycle detection, and
# its use THROUGH eval-path (a top-level `.caos-expr` invoking it).
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
# The CLI ingests only git-tracked paths, so commit before each run.
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

# The fixture: `app` depends on two libs by relative path; `lib/foo` depends on
# its sibling `lib/bar`. So `bar` is reached three ways (app's dep, foo's dep,
# and structurally under lib) — one shared node.
build_fixture() { # <root-dir>
  local r=$1
  mkdir -p "$r/app" "$r/lib/foo" "$r/lib/bar"
  echo "app main" > "$r/app/main.txt"
  printf '../lib/foo foo\n../lib/bar bar\n' > "$r/app/DEPS"
  echo "foo lib" > "$r/lib/foo/foo.txt"
  printf '../bar shared\n' > "$r/lib/foo/DEPS"
  echo "bar lib" > "$r/lib/bar/bar.txt"
}

deepen() { # <in-dir> <out-dir>
  "$CAOS_CLI" run /cas/std/deep-deps "$2" -- --in:@="$1"
}

build_fixture tree
commit "fixture"

echo "== deepened shape: DEPS replaced by DEEP-DEPS, paths resolved ==" >&2
deepen tree outA
[ -e outA/app/DEEP-DEPS/foo/foo.txt ]                 || fail "app should mount foo"
[ -e outA/app/DEEP-DEPS/bar/bar.txt ]                 || fail "app should mount bar"
[ -e outA/app/DEEP-DEPS/foo/DEEP-DEPS/shared/bar.txt ] || fail "foo should mount bar as shared"
[ -e outA/app/main.txt ]                              || fail "app's own files should survive"
[ -e outA/app/DEPS ]                                  && fail "DEPS should be dropped from nodes"
[ -e outA/lib/foo/DEEP-DEPS/shared/bar.txt ]          || fail "lib/foo should also be deepened in place"
echo "  ok: relative <path> resolved, <name> mounted, DEPS dropped" >&2

echo "== DAG sharing: bar is one identical node everywhere it appears ==" >&2
diff -r outA/app/DEEP-DEPS/bar outA/lib/bar >/dev/null \
  || fail "app's bar and lib/bar should be identical"
diff -r outA/app/DEEP-DEPS/foo outA/lib/foo >/dev/null \
  || fail "app's foo and lib/foo should be identical"
diff -r outA/app/DEEP-DEPS/foo/DEEP-DEPS/shared outA/lib/bar >/dev/null \
  || fail "foo's shared dep should be the same node as lib/bar"
echo "  ok: shared subgraphs are byte-identical" >&2

echo "== editing an unrelated package leaves app,lib untouched ==" >&2
mkdir -p tree/other; echo x > tree/other/x.txt
commit "add unrelated dir"
deepen tree outB
diff -r outA/app outB/app >/dev/null || fail "app changed after an unrelated edit"
diff -r outA/lib outB/lib >/dev/null || fail "lib changed after an unrelated edit"
[ -e outB/other/x.txt ] || fail "the new dir is missing from the output"
echo "  ok: app,lib byte-identical; other added" >&2

echo "== editing shared leaf bar recomputes everything that reaches it ==" >&2
echo "bar lib v2" > tree/lib/bar/bar.txt
commit "edit bar"
deepen tree outC
diff -r outA/app outC/app >/dev/null && fail "app should change when bar changes"
[ "$(cat outC/app/DEEP-DEPS/bar/bar.txt)" = "bar lib v2" ] || fail "app's bar not updated"
[ "$(cat outC/app/DEEP-DEPS/foo/DEEP-DEPS/shared/bar.txt)" = "bar lib v2" ] \
  || fail "foo's shared bar not updated"
echo "  ok: every node reaching bar recomputed" >&2

echo "== a dependency cycle is detected (by the server) ==" >&2
# Close a loop: lib/bar -> app, so app -> bar -> app. The recursion re-enters
# the same node request and the server's run-cycle detection catches it.
printf '../../app loop\n' > tree/lib/bar/DEPS
commit "cycle"
if deepen tree outD 2>cyc.err; then
  fail "expected the cyclic graph to fail, but the run succeeded"
fi
grep -q "run cycle detected" cyc.err || fail "no cycle reported; got: $(cat cyc.err)"
rm -f tree/lib/bar/DEPS
echo "  ok: run failed with a run-cycle error" >&2

echo "== through eval-path: a top-level .caos-expr invokes deep-deps ==" >&2
# A `.caos-expr` at the tree root deepens the whole tree; eval-path then
# descends into the deepened result.
build_fixture evtree
printf 'run /std/deep-deps -- --in:@=.\n' > evtree/.caos-expr
commit "evtree with .caos-expr"
out=$("$CAOS_CLI" eval-path evtree/app/DEEP-DEPS/foo) || fail "eval-path failed"
kind=${out%% *}; hash=${out##* }
[ "$kind" = tree ] || fail "expected a tree, got: $out"
"$CAOS_CLI" get "$hash" evfoo || fail "get $hash"
[ -e evfoo/foo.txt ] || fail "deepened foo missing foo.txt"
[ -e evfoo/DEEP-DEPS/shared/bar.txt ] || fail "deepened foo missing its shared dep"
echo "  ok: eval-path deepened the tree and dug into a package's node" >&2

echo "deep-deps: ALL PASS" >&2
