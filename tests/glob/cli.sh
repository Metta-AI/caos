#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a testenv worker (tests/lib/run-nested.sh).
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
paths() { find "$1" -type f | sed "s#^$1/##" | sort; }

mkdir workspace
cp -R test/tree/. workspace/
printf '{"pattern":"**/*.rs"}\n' > call.json
git add workspace call.json
git -c user.email=test@caos -c user.name=caos commit -qm "glob input"

echo "== recursive generic-worker glob ==" >&2
"$CAOS_CLI" run /cas/std/glob out -- --in:@=call.json --workspace:@=workspace
grep -qF '"is_error":false' out/result.json || fail "glob result marked as an error"
expected='.hidden.rs
root.rs
src/deep/mod.rs
src/lib.rs
src/main.rs'
actual=$(jq -r .content out/result.json)
[ "$actual" = "$expected" ] || fail "recursive matches wrong: $actual"
[ "$(paths out)" = 'result.json' ] || fail "glob returned non-ABI output: $(paths out)"

echo "== one star does not cross directories ==" >&2
printf '{"pattern":"src/*.rs"}\n' > call.json
git add call.json
git -c user.email=test@caos -c user.name=caos commit -qm "single-star input"
"$CAOS_CLI" run /cas/std/glob out2 -- --in:@=call.json --workspace:@=workspace
[ "$(jq -r .content out2/result.json)" = 'src/lib.rs
src/main.rs' ] || fail "single-star matches wrong"

echo "== malformed patterns are tool errors ==" >&2
printf '{"pattern":"["}\n' > call.json
git add call.json
git -c user.email=test@caos -c user.name=caos commit -qm "invalid input"
"$CAOS_CLI" run /cas/std/glob out3 -- --in:@=call.json --workspace:@=workspace
grep -qF '"is_error":true' out3/result.json || fail "invalid pattern was not an error value"
grep -qF 'invalid pattern' out3/result.json || fail "invalid-pattern error unclear"

echo "glob: ALL PASS" >&2
