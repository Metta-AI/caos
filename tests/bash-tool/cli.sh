#!/usr/bin/env bash
# Runs on the HOST (launched by tests/run.sh), cwd'd into a throwaway client
# repo with the test directory committed at ./test and $CAOS_CLI set.
#
# Exercises the bounded bash tool (worker-bash-tool, design/agent-harness.md):
# a command over a workspace tree with only the *declared* paths materialized.
# Asserts: a targeted read touches only its declared path and the result tree
# round-trips the workspace identically; an undeclared touch fails with EACCES
# and a structured `denied` retry hint; writes stage back correctly with
# untouched placeholder subtrees intact by hash; and a failing command is a
# VALUE ({exit, stdout, stderr, tree}), never a run error.
#
# The tool is the worker-bash-tool static binary run as curry(runner, bin) in
# the shared runner pool — no image of its own.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
# The git tree hash of a (possibly untracked) path, via a snapshot commit —
# content-addressed, so equal content means equal hash.
snap() { commit "snap" >/dev/null 2>&1 || true; git rev-parse "HEAD:$1"; }

# The workspace under test: two levels, so undeclared subtrees stay placeholders.
mkdir -p ws/a/b
echo one > ws/a/one.txt
echo two > ws/a/b/two.txt
echo top > ws/top.txt

# The tool binary, bound into the shared runner: prebuilt when the harness
# provides binaries (CAOS_BIN_DIR — the nested runner does), else the flake.
if [ -n "${CAOS_BIN_DIR:-}" ]; then
  cp "$CAOS_BIN_DIR/worker-bash-tool" bash-tool-bin
else
  nix build "$CAOS_PROJECT#worker-bash-tool" -o bash-tool-out
  cp -L bash-tool-out/bin/worker-bash-tool bash-tool-bin
  rm bash-tool-out
fi
commit "workspace + bash tool binary"
base=$(git rev-parse HEAD)
tool=$("$CAOS_CLI" curry /cas/std/runner -- --bin:@=bash-tool-bin)

echo "== targeted read: declared path only; workspace round-trips by hash ==" >&2
"$CAOS_CLI" run "$tool" r1 -- --tree:@=ws --cmd='cat a/one.txt' --paths='a/one.txt'
[ "$(cat r1/exit)" = "0" ] || fail "read: exit $(cat r1/exit)"
[ "$(cat r1/stdout)" = "one" ] || fail "read: stdout $(cat r1/stdout)"
[ "$(snap r1/tree)" = "$(git rev-parse "$base:ws")" ] \
  || fail "read-only run changed the workspace tree"
echo "  ok: read its file; tree unchanged (identical hash)" >&2

echo "== undeclared touch: EACCES + structured retry hint ==" >&2
"$CAOS_CLI" run "$tool" r2 -- --tree:@=ws --cmd='cat a/b/two.txt' --paths='top.txt'
[ "$(cat r2/exit)" != "0" ] || fail "undeclared read did not fail"
grep -qi "permission denied" r2/stderr || fail "no EACCES in stderr: $(cat r2/stderr)"
[ -f r2/denied ] || fail "no denied hint in the result"
grep -q "a/b/two.txt" r2/denied || fail "hint misses the path: $(cat r2/denied)"
echo "  ok: EACCES surfaced, denied names a/b/two.txt" >&2

echo "== writes staged back; untouched placeholder subtree intact by hash ==" >&2
"$CAOS_CLI" run "$tool" r3 -- --tree:@=ws \
  --cmd='echo hi > new.txt && echo edited >> a/one.txt' --paths='a/one.txt'
[ "$(cat r3/exit)" = "0" ] || fail "write: exit $(cat r3/exit)"
[ "$(cat r3/tree/new.txt)" = "hi" ] || fail "created file missing/wrong"
[ "$(cat r3/tree/a/one.txt)" = "$(printf 'one\nedited')" ] || fail "edit not staged"
[ "$(snap r3/tree/a/b)" = "$(git rev-parse "$base:ws/a/b")" ] \
  || fail "untouched subtree a/b did not round-trip by hash"
[ "$(cat r3/tree/top.txt)" = "top" ] || fail "untouched top.txt lost"
echo "  ok: new.txt + edit staged, a/b round-tripped" >&2

echo "== a failing command is a value, not a run error ==" >&2
"$CAOS_CLI" run "$tool" r4 -- --tree:@=ws --cmd='echo oops >&2; exit 7'
[ "$(cat r4/exit)" = "7" ] || fail "exit code not surfaced: $(cat r4/exit)"
grep -q "oops" r4/stderr || fail "stderr not captured"
[ "$(snap r4/tree)" = "$(git rev-parse "$base:ws")" ] || fail "failed run mangled the tree"
echo "  ok: exit 7 + stderr returned as a value" >&2

echo "bash-tool: ALL PASS" >&2
