#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set — normally INSIDE a testenv worker, as the suite's per-test job
# (tests/lib/run-nested.sh); tests/run.sh runs it on the host against the
# outer stack for interactive debugging.
#
# Round-trips a first-class commit: the client passes HEAD as a `:commit=` arg
# (unpeeled — the worker sees the commit, not its tree); a source-built worker
# (commit-worker.rs, compiled by the rustc builder, linking worker-common's
# commit helpers) reads it, walks its tree and parent by hash, runs one tool
# call through run-then, and mints a child commit — message from the tool's
# output, tree unchanged, parent = HEAD — returned as `commit <hash>`. The
# client gets the raw commit bytes on stdout and can fetch the real object
# from the server by hash.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

echo "== build the commit worker from source ==" >&2
builder=$("$CAOS_CLI" curry /cas/std/rustc -- --runner:@=/cas/std/runner)
"$CAOS_CLI" run "$builder" img -- --src:@=test/commit-worker.rs
commit "built worker image"
worker=$(git rev-parse HEAD:img)

# The conversation head this run is over: the current HEAD (which now includes
# the built image, so it has real history and content behind it).
head=$(git rev-parse HEAD)
head_tree=$(git rev-parse 'HEAD^{tree}')

echo "== HEAD as a :commit= arg -> worker -> child commit on stdout ==" >&2
"$CAOS_CLI" run "$worker" -- --head:commit=HEAD --tool-script:@=test/tool.sh \
  > child.commit
grep -q "^tree $head_tree\$" child.commit \
  || fail "child commit does not snapshot HEAD's tree: $(cat child.commit)"
grep -q "^parent $head\$" child.commit \
  || fail "child commit's parent is not HEAD: $(cat child.commit)"
grep -q "tool said 42" child.commit \
  || fail "message doesn't carry the tool output: $(cat child.commit)"
echo "  ok: child commit has HEAD as parent, HEAD's tree, and the tool output" >&2

echo "== the minted commit is fetchable from the server as a real commit ==" >&2
hash=$(git hash-object -t commit --stdin < child.commit)
# noop negotiation, as the CLI itself uses: single-round fetch by bare hash.
git -c fetch.negotiationAlgorithm=noop fetch --quiet caos "$hash"
[ "$(git cat-file -t "$hash")" = "commit" ] || fail "$hash is not a commit"
[ "$(git rev-parse "$hash^{tree}")" = "$head_tree" ] || fail "fetched tree differs"
[ "$(git rev-parse "$hash^")" = "$head" ] || fail "fetched parent differs"
git cat-file commit "$hash" | grep -q "tool said 42" || fail "fetched message differs"
echo "  ok: fetched $hash and verified tree/parent/message with plain git" >&2

echo "commit: ALL PASS" >&2
