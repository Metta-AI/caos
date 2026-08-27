#!/bin/bash
# tests/commit — a WORKER test: no client, no repo.
#
# Round-trips a first-class commit: a commit is passed as a `:commit=` arg
# (unpeeled — the worker sees the commit, not its tree); a source-built worker
# (commit-worker.rs, compiled by the rustc builder here, linking worker-common's
# commit helpers) reads it, walks its tree and parent by hash, runs one tool
# call through run-then, and mints a child commit — message from the tool's
# output, tree unchanged, parent = the head — returned as `commit <hash>`.
#
# GIT WAS A LOCAL TOOL HERE. It made the head commit and read the child back,
# and a worker has both: `caos put-commit` mints a commit (validated here and
# again by the server), and a `commit`-kind result is materialized as those raw
# object bytes — so `tree`/`parent`/the message are lines to grep. The final
# claim, that the minted commit is a REAL object on the server rather than
# bytes on stdout, is `caos get-hash` on its oid: that fetches by hash from the
# server and would fail outright if nothing were stored.
#
# THREE STAGES: no run can be waited on, so each assertion is the `then` of the
# run it is about.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --rustc:@=/cas/args/rustc --cworker:@=/cas/args/cworker \
  --tool:@=/cas/args/tool "$@"; }

caos get /cas/args/test-salt || fail "reading --test-salt"
SALT=$(cat /cas/args/test-salt)
TS=1700000000

result_commit() { caos get /cas/args/result >/dev/null; cat /cas/args/result; }
commit_field() { printf '%s\n' "$1" | grep -m1 "^$2 " | cut -d' ' -f2- ; }

case "$stage" in

start)
  echo "== build the commit worker from source ==" >&2
  # No runner is named: rustc DEPENDS on the runner pool (std/rustc/DEPS) and
  # curries the built binary onto it itself, so a caller says only what it is
  # building.
  caos get /cas/args/cworker || fail "reading commit-worker.rs"
  caos run-request-then \
    "$(caos prepare-request --base:hash="$(caos hash /cas/args/rustc)" \
        --src:@=/cas/args/cworker)" \
    --then:hash="$(next built)"
  ;;

built)
  worker=$(caos hash /cas/args/result)
  echo "  built commit-worker image $worker" >&2

  echo "== a commit as a :commit= arg -> worker -> child commit ==" >&2
  # The head this run is over. Its tree is a real tree with content behind it,
  # so "the child snapshots the head's tree" is a claim about an actual object.
  rm -rf /tmp/ws; mkdir -p /tmp/ws
  printf 'workspace survives\n' > /tmp/ws/workspace.txt
  caos put /tmp/ws /cas/ws >/dev/null || fail "publishing the head's tree"
  head_tree=$(caos hash /cas/ws)
  # TWO COMMITS, because the worker WALKS ONE GENERATION: it reads the head's
  # parent by hash, so a root commit is rejected outright ("head commit has no
  # parent"). The root is what gives it one.
  mint() { # <dst> <message> [parent] -> the commit hash
    local dst=$1 msg=$2 parent=${3:-}
    { printf 'tree %s\n' "$head_tree"
      if [ -n "$parent" ]; then printf 'parent %s\n' "$parent"; fi
      printf 'author caos <test@caos> %s +0000\n' "$TS"
      printf 'committer caos <test@caos> %s +0000\n' "$TS"
      printf '\n%s (%s)\n' "$msg" "$SALT"
    } > /tmp/commit
    caos put-commit /tmp/commit "$dst" || fail "minting $dst"
  }
  root=$(mint /cas/root "conversation root")
  head=$(mint /cas/head "conversation base" "$root")

  # --bash: the worker curries its tool onto the bash image, and a worker cannot
  # evaluate a `.caos-expr` — so it is resolved here and the hash bound.
  caos get /cas/args/tool || fail "reading tool.sh"
  caos run-request-then \
    "$(caos prepare-request --base:hash="$worker" \
        --head:@=/cas/head --tool-script:@=/cas/args/tool \
        --bash="$(caos hash /cas/args/base)")" \
    --then:hash="$(next child --head="$head" --head-tree="$head_tree")"
  ;;

child)
  caos get /cas/args/head; caos get /cas/args/head-tree
  head=$(cat /cas/args/head); head_tree=$(cat /cas/args/head-tree)
  child=$(result_commit)
  [ "$(commit_field "$child" tree)" = "$head_tree" ] \
    || fail "child commit does not snapshot the head's tree: $child"
  [ "$(commit_field "$child" parent)" = "$head" ] \
    || fail "child commit's parent is not the head: $child"
  printf '%s\n' "$child" | grep -q "tool said 42" \
    || fail "message doesn't carry the tool output: $child"
  echo "  ok: child has the head as parent, the head's tree, and the tool output" >&2

  echo "== the minted commit is a real commit on the server ==" >&2
  # By OID, fetched back from the server — the result arrived as bytes, and this
  # is what proves an object was stored rather than merely printed.
  hash=$(caos hash /cas/args/result)
  caos get-hash "$hash" /cas/refetched || fail "$hash is not fetchable from the server"
  refetched=$(cat /cas/refetched)
  [ "$(commit_field "$refetched" tree)" = "$head_tree" ] || fail "fetched tree differs"
  [ "$(commit_field "$refetched" parent)" = "$head" ] || fail "fetched parent differs"
  printf '%s\n' "$refetched" | grep -q "tool said 42" || fail "fetched message differs"
  echo "  ok: fetched $hash and verified tree/parent/message" >&2

  printf 'commit: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
