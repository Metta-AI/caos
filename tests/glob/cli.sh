#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a testenv worker (tests/lib/run-nested.sh).
#
# Exercises the glob worker directly: recursive `**`, separator-aware `*`,
# alternates, an empty result, and a malformed-pattern failure.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

paths() {
  find "$1" -type f | sed "s#^$1/##" | sort
}

echo "== recursive glob ==" >&2
"$CAOS_CLI" run /cas/std/glob out -- --pattern='**/*.rs' --in:@=test/tree
[ "$(paths out)" = '.hidden.rs
root.rs
src/deep/mod.rs
src/lib.rs
src/main.rs' ] || fail "recursive matches wrong: $(paths out)"

echo "== a single star does not cross directories ==" >&2
"$CAOS_CLI" run /cas/std/glob out2 -- --pattern='src/*.rs' --in:@=test/tree
[ "$(paths out2)" = 'src/lib.rs
src/main.rs' ] || fail "single-star matches wrong: $(paths out2)"

echo "== alternates ==" >&2
"$CAOS_CLI" run /cas/std/glob out3 -- \
  --pattern='src/**/{lib,mod}.rs' --in:@=test/tree
[ "$(paths out3)" = 'src/deep/mod.rs
src/lib.rs' ] || fail "alternate matches wrong: $(paths out3)"

echo "== no matches is an empty tree ==" >&2
"$CAOS_CLI" run /cas/std/glob out4 -- --pattern='**/*.py' --in:@=test/tree
[ -z "$(paths out4)" ] || fail "no-match result is not empty: $(paths out4)"

echo "== malformed patterns fail clearly ==" >&2
if "$CAOS_CLI" run /cas/std/glob -- --pattern='[' --in:@=test/tree 2>err; then
  fail "malformed pattern succeeded"
fi
grep -qF 'invalid pattern' err || fail "malformed-pattern error unclear: $(cat err)"

echo "glob: ALL PASS" >&2
