#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI set,
# INSIDE the dev stack — the suite's per-test job (tests/lib stages the repo, then runs this).
#
# `std/flake-input-loader` (design/flake-inputs.md, "Consumer root"): a project
# that is NOT caos mounts a PINNED input into its own evaluated tree, with
# nothing of that input committed. The consumer's root `.caos-expr` says which
# input, what of it, and where it goes; the loader checks the pin against
# `flake.lock` and splices.
#
# The fixture is a synthetic input repo reached over `git+file://` — the subject
# is the loader, not anyone's TLS. What it must show:
#
#   - the input tree lands at `--output-path`, creating parent directories that
#     the consumer's tree does not have;
#   - every sibling survives the splice (it is a merge, not a replacement);
#   - the mounted tree is IDENTICAL to the input repo's own subtree, i.e. the
#     locator resolved to content and nothing was rebuilt;
#   - a pin that DRIFTS from flake.lock is refused — the `nix flake update`
#     without regenerating case, which is the whole reason for the check.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

# ---- the input repo, standing in for caos ------------------------------------
SRC=/tmp/fil-src
rm -rf "$SRC"; mkdir -p "$SRC/std/thing"
git init -q "$SRC"
git -C "$SRC" config uploadpack.allowReachableSHA1InWant true
printf 'from the input\n' > "$SRC/std/thing/file"
git -C "$SRC" add -A
git -C "$SRC" -c user.email=test@caos -c user.name=caos commit -qm one
OLD=$(git -C "$SRC" rev-parse HEAD)
# A second commit, so the drift case can pin a rev that EXISTS (a bogus sha
# would fail in the client's fetch, before the loader ever ran, and would prove
# nothing about the check).
printf 'changed\n' > "$SRC/std/thing/file"
git -C "$SRC" add -A
git -C "$SRC" -c user.email=test@caos -c user.name=caos commit -qm two
SHA=$(git -C "$SRC" rev-parse HEAD)
REPO="git+file://$SRC"

# ---- a consumer package ------------------------------------------------------
# The loader is reached by a local mount here; `tests/remote-ref` already covers
# reaching a worker by locator, and this test is about what the loader DOES.
mk_consumer() { # <dir> <rev the expression pins>
  local dir=$1 rev=$2
  mkdir -p "$dir"
  cp -r DEEP-DEPS/flake-input-loader "$dir/loader"
  printf '{ inputs.demo.url = "git+file://%s"; outputs = _: { }; }\n' "$SRC" > "$dir/flake.nix"
  cat > "$dir/flake.lock" <<LOCK
{ "nodes": { "root":   { "inputs": { "demo": "demo" } },
             "demo":   { "locked": { "type": "git", "url": "file://$SRC", "rev": "$SHA" } } },
  "root": "root", "version": 7 }
LOCK
  printf 'keep me\n' > "$dir/sibling.txt"
  cat > "$dir/.caos-expr" <<EXPR
run --base:@=loader --in:@=. --expr=\$CAOS_EXPR --input=demo --input-tree:@@=$REPO?rev=$rev&dir=std --output-path=vendor/demo-std
EXPR
}
mk_consumer pkg "$SHA"
mk_consumer pkg-drift "$OLD"
git add -A && git -c user.email=test@caos -c user.name=caos commit -qm flake-input-loader

echo "== a pinned input is mounted into the consumer's evaluated tree ==" >&2
out=$("$CAOS_CLI" eval-path pkg) || fail "eval-path pkg failed: $out"
[ "${out%% *}" = tree ] || fail "expected a tree result, got: $out"
"$CAOS_CLI" get "${out##* }" got || fail "get ${out##* }"

[ "$(cat got/vendor/demo-std/thing/file)" = changed ] \
  || fail "mounted content: $(cat got/vendor/demo-std/thing/file 2>&1)"
echo "  ok: the input landed at vendor/demo-std, parent dirs created" >&2

# A merge, not a replacement — and the directive is gone, because the expression
# was evaluated against its directory MINUS itself.
[ "$(cat got/sibling.txt)" = "keep me" ] || fail "a sibling was lost in the splice"
[ -e got/flake.lock ] || fail "flake.lock was lost in the splice"
[ ! -e got/.caos-expr ] || fail ".caos-expr came back in the result"
echo "  ok: siblings survived; the directive did not" >&2

# The mounted tree IS the input repo's own subtree: the locator resolved to
# content, nothing was rebuilt or copied.
mounted=$(cd got/vendor/demo-std && git -C "$SRC" rev-parse "$SHA:std")
have=$("$CAOS_CLI" eval-path pkg/vendor/demo-std) || fail "eval-path into the mount failed"
[ "${have##* }" = "$mounted" ] \
  || fail "the mount is ${have##* }, not the input's own std tree $mounted"
echo "  ok: the mount is the input repo's own tree oid" >&2

echo "== a pin that drifts from flake.lock is refused ==" >&2
if "$CAOS_CLI" eval-path pkg-drift >/dev/null 2>/tmp/err; then
  fail "a drifted pin was accepted"
fi
grep -q "is locked at $SHA in flake.lock" /tmp/err \
  || fail "wrong error for a drifted pin: $(cat /tmp/err)"
grep -q "but the expression pins $OLD" /tmp/err \
  || fail "the drift error did not name the expression's rev: $(cat /tmp/err)"
echo "  ok: the loader named both revisions and refused" >&2
