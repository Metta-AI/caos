#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack — the suite's per-test job
# (tests/lib/run-test.sh).
#
# Exercises `caos-cli eval-path` (design/caos-expr.md): a `.caos-expr` file
# makes its directory evaluable, and `eval-path <path>` walks the workspace
# tree from the root down to <path>, evaluating each `.caos-expr` in the tree
# its parent produced and descending into the result. The fixtures build a
# directory by running a curried `std/bash` worker over the directory's own
# files, so no new images are needed. Covered: a `run`-valued expression, the
# `NAME=`/`$NAME` variable form (which must converge on the same result),
# descending PAST an expression into its result, and cache-hit determinism.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

# A package whose `.caos-expr` builds its own directory: run the bash worker
# over the package's files, producing a tree with a `greeting` file.
mkdir -p pkg-direct
cat > pkg-direct/build.sh <<'EOF'
#!/bin/bash
set -euo pipefail
caos get /cas/args/name
mkdir -p /tmp/out
echo "hello $(cat /cas/args/name)" > /tmp/out/greeting
caos put /tmp/out /cas/out
EOF
echo world > pkg-direct/name
cat > pkg-direct/.caos-expr <<'EOF'
# build this directory by running the bash worker over its files
run /std/bash -- --worker1:@=build.sh --name:@=name
EOF

# The same computation written with a bound variable: curry the worker in,
# then run it. It must produce a byte-identical arg tree, hence the same
# result hash.
mkdir -p pkg-var
cp pkg-direct/build.sh pkg-var/build.sh
cp pkg-direct/name pkg-var/name
cat > pkg-var/.caos-expr <<'EOF'
G=curry /std/bash -- --worker1:@=build.sh
run $G -- --name:@=name
EOF

commit "eval-path fixtures"

echo "== eval-path evaluates a directory's .caos-expr ==" >&2
out=$("$CAOS_CLI" eval-path pkg-direct) || fail "eval-path pkg-direct failed"
kind=${out%% *}; hash=${out##* }
[ "$kind" = tree ] || fail "expected a tree result, got: $out"
"$CAOS_CLI" get "$hash" got-direct || fail "get $hash"
[ "$(cat got-direct/greeting)" = "hello world" ] \
  || fail "greeting: $(cat got-direct/greeting)"
echo "  ok: pkg-direct -> tree with greeting='hello world'" >&2

echo "== the variable / curry form evaluates identically ==" >&2
out2=$("$CAOS_CLI" eval-path pkg-var) || fail "eval-path pkg-var failed"
[ "${out2##* }" = "$hash" ] || fail "var form differs: $out2 vs tree $hash"
echo "  ok: 'NAME=curry ... ; run \$NAME' converges on the same result" >&2

echo "== descending past the expression digs into its result ==" >&2
g=$("$CAOS_CLI" eval-path pkg-direct/greeting) || fail "eval-path pkg-direct/greeting"
gkind=${g%% *}; ghash=${g##* }
[ "$gkind" = blob ] || fail "expected a blob, got: $g"
"$CAOS_CLI" get "$ghash" got-greeting || fail "get greeting blob"
[ "$(cat got-greeting)" = "hello world" ] || fail "greeting blob: $(cat got-greeting)"
echo "  ok: eval-path pkg-direct/greeting -> the produced blob" >&2

echo "== a repeated eval is a cache hit (same hash) ==" >&2
out3=$("$CAOS_CLI" eval-path pkg-direct)
[ "${out3##* }" = "$hash" ] || fail "rerun differs: $out3 vs $hash"
echo "  ok: identical eval -> identical result" >&2

echo "eval-path: ALL PASS" >&2
