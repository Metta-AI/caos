#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE the dev stack — the suite's per-test job
# (tests/lib stages the repo, then runs this).
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

# The bash entry this test's DEPS declared, copied into each fixture. An image
# named in a `.caos-expr` is a path WITHIN that subtree — the fixture cannot
# reach out of itself — so a real package would mount it via DEPS and get
# exactly this. There is no ambient `/std/<name>` to name instead.
BASH=DEEP-DEPS/bash

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
cp -r "$BASH" pkg-direct/bash
cat > pkg-direct/.caos-expr <<'EOF'
# build this directory by running the bash worker over its files
run --base:@=bash --worker1:@=build.sh --name:@=name
EOF

# The same computation written with a bound variable: curry the worker in,
# then run it. It must produce a byte-identical arg tree, hence the same
# result hash.
mkdir -p pkg-var
cp pkg-direct/build.sh pkg-var/build.sh
cp pkg-direct/name pkg-var/name
cp -r "$BASH" pkg-var/bash
cat > pkg-var/.caos-expr <<'EOF'
G=curry --base:@=bash --worker1:@=build.sh
run --base=$G --name:@=name
EOF

# The same computation with the worker described by a SUBTREE referenced as the
# image by PATH: `tool/` carries its own `.caos-expr` (a curry over bash), and
# `run tool` resolves that path THROUGH the subtree's expression — the mechanism
# a core item uses to name a dependency by a local `DEEP-DEPS/<name>` mount
# (design/caos-expr.md). It must again produce the
# byte-identical arg tree, hence the same result hash.
mkdir -p pkg-nested/tool
cp pkg-direct/build.sh pkg-nested/tool/build.sh
cp pkg-direct/name pkg-nested/name
cp -r "$BASH" pkg-nested/tool/bash
cat > pkg-nested/tool/.caos-expr <<'EOF'
curry --base:@=bash --worker1:@=build.sh
EOF
cat > pkg-nested/.caos-expr <<'EOF'
run --base:@=tool --name:@=name
EOF

# A `:@=` ARG whose target carries a `.caos-expr` is an EXPRESSION, so it is
# evaluated and the arg binds what it BUILT — not the source directory
# (design/caos-expr.md). `dep/` produces a tree holding `name`; the outer
# worker reads `/cas/args/namedir/name`, which exists only in dep's RESULT
# (the raw dir holds `gen.sh`, `bash` and `.caos-expr` instead). So this
# asserts evaluation happened, not just that something was bound.
mkdir -p pkg-argexpr/dep
cp -r "$BASH" pkg-argexpr/bash
cp -r "$BASH" pkg-argexpr/dep/bash
cat > pkg-argexpr/dep/gen.sh <<'EOF'
#!/bin/bash
set -euo pipefail
mkdir -p /tmp/out
echo world > /tmp/out/name
caos put /tmp/out /cas/out
EOF
cat > pkg-argexpr/dep/.caos-expr <<'EOF'
run --base:@=bash --worker1:@=gen.sh
EOF
cat > pkg-argexpr/read.sh <<'EOF'
#!/bin/bash
set -euo pipefail
# Two gets: `caos get` materializes ONE level, so a nested blob needs its own —
# without the second, `name` is an unmaterialized placeholder that reads EMPTY
# rather than failing, which looks exactly like "the arg was bound raw".
caos get /cas/args/namedir
caos get /cas/args/namedir/name
mkdir -p /tmp/out
# Carried into the RESULT, not echoed to stderr: a passing worker's stderr never
# reaches the test record, so a diagnostic only helps if it is part of the output.
ls -a /cas/args/namedir | tr '\n' ' ' > /tmp/out/listing
echo "hello $(cat /cas/args/namedir/name)" > /tmp/out/greeting
caos put /tmp/out /cas/out
EOF
cat > pkg-argexpr/.caos-expr <<'EOF'
run --base:@=bash --worker1:@=read.sh --namedir:@=dep
EOF

# A `:@=` target with NO `.caos-expr` is DATA and stays raw — the other half of
# the same rule. `data/` is a plain directory; the worker must see its literal
# file, so binding it must NOT go looking for an expression to run.
mkdir -p pkg-argdata/data
cp -r "$BASH" pkg-argdata/bash
echo world > pkg-argdata/data/name
cp pkg-argexpr/read.sh pkg-argdata/read.sh
cat > pkg-argdata/.caos-expr <<'EOF'
run --base:@=bash --worker1:@=read.sh --namedir:@=data
EOF

# `$CAOS_EXPR` — the expression's OWN blob. The point is that this is the only
# way to get it: the directive is stripped from the tree the expression is
# evaluated against, so `--expr:@=.caos-expr` names a file that is not there.
# A worker that must VERIFY the expression which launched it (the consumer-input
# expander checking its locators against `flake.lock` — design/flake-inputs.md)
# has no other route.
mkdir -p pkg-self
cat > pkg-self/show.sh <<'EOF'
#!/bin/bash
set -euo pipefail
caos get /cas/args/expr
mkdir -p /tmp/out
cp /cas/args/expr /tmp/out/expr
if [ -e /cas/args/in/.caos-expr ]; then
  echo yes > /tmp/out/stripped-in
else
  echo no > /tmp/out/stripped-in
fi
caos put /tmp/out /cas/out
EOF
cp -r "$BASH" pkg-self/bash
cat > pkg-self/.caos-expr <<'EOF'
run --base:@=bash --worker1:@=show.sh --in:@=. --expr=$CAOS_EXPR
EOF

# A HERE-STRING binds a literal blob inline (design/caos-expr.md). `--name=$NAME`
# where NAME's body is `world` is byte-identical to pkg-direct's `--name:@=name`
# (the `name` file is `echo world` = "world\n"), so the whole arg tree — and its
# result hash — must match pkg-direct. This asserts a here-string is exactly the
# `--k=value`/`:@=file` blob, only authored inline.
mkdir -p pkg-here
cp pkg-direct/build.sh pkg-here/build.sh
cp -r "$BASH" pkg-here/bash
cat > pkg-here/.caos-expr <<'EOF'
NAME=<<END
world
END
run --base:@=bash --worker1:@=build.sh --name=$NAME
EOF

# A here-string in IMAGE position is an error: a blob is not an image.
mkdir -p pkg-here-bad
cp pkg-direct/build.sh pkg-here-bad/build.sh
cp pkg-direct/name pkg-here-bad/name
cp -r "$BASH" pkg-here-bad/bash
cat > pkg-here-bad/.caos-expr <<'EOF'
IMG=<<END
not-an-image
END
run --base=$IMG --name:@=name
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

echo "== an image referenced by path evaluates that subtree's .caos-expr ==" >&2
out_nested=$("$CAOS_CLI" eval-path pkg-nested) || fail "eval-path pkg-nested failed"
[ "${out_nested##* }" = "$hash" ] \
  || fail "path-image form differs: $out_nested vs tree $hash"
echo "  ok: 'run tool' resolved tool/'s .caos-expr and converged on the same result" >&2

echo "== descending past the expression digs into its result ==" >&2
g=$("$CAOS_CLI" eval-path pkg-direct/greeting) || fail "eval-path pkg-direct/greeting"
gkind=${g%% *}; ghash=${g##* }
[ "$gkind" = blob ] || fail "expected a blob, got: $g"
"$CAOS_CLI" get "$ghash" got-greeting || fail "get greeting blob"
[ "$(cat got-greeting)" = "hello world" ] || fail "greeting blob: $(cat got-greeting)"
echo "  ok: eval-path pkg-direct/greeting -> the produced blob" >&2

echo "== a :@= arg naming an EXPRESSION binds what it built ==" >&2
out_arg=$("$CAOS_CLI" eval-path pkg-argexpr) || fail "eval-path pkg-argexpr failed"
[ "${out_arg%% *}" = tree ] || fail "expected a tree, got: $out_arg"
"$CAOS_CLI" get "${out_arg##* }" got-argexpr || fail "get ${out_arg##* }"
[ "$(cat got-argexpr/greeting)" = "hello world" ] \
  || fail "argexpr greeting: '$(cat got-argexpr/greeting)' — namedir held: $(cat got-argexpr/listing)"
echo "  ok: --namedir:@=dep resolved through dep/'s .caos-expr" >&2

echo "== a :@= arg naming plain DATA stays raw ==" >&2
out_dat=$("$CAOS_CLI" eval-path pkg-argdata) || fail "eval-path pkg-argdata failed"
"$CAOS_CLI" get "${out_dat##* }" got-argdata || fail "get ${out_dat##* }"
[ "$(cat got-argdata/greeting)" = "hello world" ] \
  || fail "argdata greeting: $(cat got-argdata/greeting)"
echo "  ok: a target with no .caos-expr is referenced as-is" >&2

echo "== \$CAOS_EXPR hands a worker the expression that launched it ==" >&2
out_self=$("$CAOS_CLI" eval-path pkg-self) || fail "eval-path pkg-self failed"
"$CAOS_CLI" get "${out_self##* }" got-self || fail "get ${out_self##* }"
[ "$(cat got-self/expr)" = "$(cat pkg-self/.caos-expr)" ] \
  || fail "the worker saw a different expression:
$(cat got-self/expr)"
# ...and the same run proves WHY it is needed: the directive is absent from the
# tree that same expression was evaluated against, so no path could name it.
[ "$(cat got-self/stripped-in)" = no ] \
  || fail "the .caos-expr was present in --in:@=. — the stripping rule changed"
echo "  ok: the worker read its own directive, which --in:@=. does not carry" >&2

echo "== a repeated eval is a cache hit (same hash) ==" >&2
out3=$("$CAOS_CLI" eval-path pkg-direct)
[ "${out3##* }" = "$hash" ] || fail "rerun differs: $out3 vs $hash"
echo "  ok: identical eval -> identical result" >&2

echo "== a here-string --k=\$NAME is the byte-identical blob to :@=file ==" >&2
out_here=$("$CAOS_CLI" eval-path pkg-here) || fail "eval-path pkg-here failed"
[ "${out_here##* }" = "$hash" ] \
  || fail "here-string form differs: $out_here vs tree $hash"
echo "  ok: --name=\$NAME (here-string 'world') converged on pkg-direct's result" >&2

echo "== a here-string in image position is rejected ==" >&2
if "$CAOS_CLI" eval-path pkg-here-bad 2>here-bad.err; then
  fail "eval-path pkg-here-bad should have failed"
fi
grep -qF 'here-string' here-bad.err \
  || fail "expected a here-string/image error, got: $(cat here-bad.err)"
echo "  ok: \$IMG (a blob) rejected as an image" >&2

echo "eval-path: ALL PASS" >&2
