#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI set,
# INSIDE the dev stack — the suite's per-test job (dev/cli-test stages the repo, then runs this).
#
# `--name:@@=<git ref>`: a tree that lives in ANOTHER repo, pinned by a commit
# sha (design/flake-inputs.md). The claim under test is that the locator is a
# FETCH COORDINATE AND NOTHING ELSE — `url + rev` becomes an oid at eval time,
# and what enters the ArgTree (so the cache key) is that oid, byte-for-byte what
# a local `:@=` of the same content would have produced. Every assertion below
# pins that down: same oid as the foreign repo's own, same oid as the local
# path, and still resolvable once the remote is gone.
#
# **THE SERVER DOES THE FETCHING**, which is the half that changed. The
# content-addressing argument is untouched — the locator still becomes an oid
# before the ArgTree carrying it exists — but the client no longer needs a
# checkout, a credential or a network path to the foreign repo, and neither does
# a worker. That is what lets an agent's SERVER-SIDE evaluation follow a pin at
# all, which is the whole point: a conversation started from a client repo has
# no source tree on anyone's disk to fall back to.
#
# A REAL FETCH OVER THE NETWORK, deliberately. The fixtures are served by `git
# daemon` on this container's own address — the same trick `chat-offline` uses
# for its stub LLM, reachable from the stack because a job's containers sit in
# the stack's netns. This used to be a `git+file://` URL, which no longer proves
# anything: the server is a different container, so a path here is not a path
# there, and that asymmetry IS the behaviour under test. The one thing a real
# host adds is `uploadpack.allowReachableSHA1InWant` — GitHub sets it, which is
# exactly why a locator pins a COMMIT and selects within it with `dir=` rather
# than naming a subtree hash — so the fixtures set it too.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }
fixture() { git -C "$1" -c user.email=test@caos -c user.name=caos "${@:2}"; }

# ---- the local half of the comparison ---------------------------------------
mkdir -p payload
printf 'from another repo\n' > payload/note.txt
commit remote-ref-ws
WS_SHA=$(git rev-parse HEAD)

# ---- two repositories the SERVER can reach ----------------------------------
SRCBASE=/tmp/remote-ref-serve
SRC=$SRCBASE/src
rm -rf "$SRCBASE"; mkdir -p "$SRC/payload" "$SRC/tool"

# This repo, reachable the same way — what the consumer fixture pins CAOS back
# to, so the test needs no second machine to play the role of caos' own host.
git clone -q --bare . "$SRCBASE/ws"
git -C "$SRCBASE/ws" config uploadpack.allowReachableSHA1InWant true

# The foreign repo, which is all a `:@@=` ever names.
git init -q "$SRC"
git -C "$SRC" config uploadpack.allowReachableSHA1InWant true
fixture "$SRC" commit --allow-empty -qm remote-ref-parent
PARENT=$(git -C "$SRC" rev-parse HEAD)
# The same BYTES as ./payload, reached the other way. Two files and a subtree, so
# the run below covers a subtree, a blob and the whole root in one container.
printf 'from another repo\n' > "$SRC/payload/note.txt"
printf 'not this one\n' > "$SRC/decoy.txt"

# ---- serve them where the SERVER can fetch from -----------------------------
# Before the fixtures' expressions are written, because those name the PORT, and
# the port is only settled once something is listening on it.
[ -n "${CAOS_STUB_HOST:-}" ] || fail "dev/cli-test did not supply CAOS_STUB_HOST"
daemon_pid=""
stop_daemon() {
  if [ -n "$daemon_pid" ]; then kill "$daemon_pid" 2>/dev/null || true; fi
  daemon_pid=""
}
trap stop_daemon EXIT
for _ in 1 2 3 4 5; do
  port=$((20000 + RANDOM % 20000))
  git daemon --reuseaddr --export-all --listen=0.0.0.0 --port="$port" \
    --base-path="$SRCBASE" "$SRCBASE" >/tmp/daemon.log 2>&1 &
  daemon_pid=$!
  for _ in $(seq 1 50); do
    if ! kill -0 "$daemon_pid" 2>/dev/null; then break; fi
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then exec 3>&-; break 2; fi
    sleep 0.1
  done
  stop_daemon
done
[ -n "$daemon_pid" ] || fail "could not start git daemon: $(cat /tmp/daemon.log)"
SERVE="git+git://$CAOS_STUB_HOST:$port"
REPO="$SERVE/src"

# THE CONSUMER STORY, in miniature: a repo that is NOT caos pins caos by locator
# and curries its own worker onto caos' std/bash. Nothing of caos is committed
# here — the pin is the whole dependency.
cat > "$SRC/tool/.caos-expr" <<EXPR
# A consumer's entry: caos' std/bash by pinned locator, its own script curried on.
curry --base:@@=$SERVE/ws?rev=$WS_SHA&dir=DEEP-DEPS/bash --worker1:@=run.sh
EXPR
cat > "$SRC/tool/run.sh" <<'RUN'
#!/bin/bash
set -euo pipefail
caos get -r /cas/args/in
printf 'consumer worker saw: %s\n' "$(cat /cas/args/in/note.txt)" > /tmp/out
caos put /tmp/out /cas/out
RUN
git -C "$SRC" add -A
fixture "$SRC" commit -qm remote-ref-src
SHA=$(git -C "$SRC" rev-parse HEAD)

echo "== the locator becomes an oid before the ArgTree exists ==" >&2
# The tightest possible statement of the claim, and it runs no worker: `curry`
# only BUILDS an ArgTree, so what this inspects is the arg entry itself.
node=$("$CAOS_CLI" curry --base:docker=unused "--x:@@=$REPO?rev=$SHA&dir=payload") \
  || fail "resolving a locator failed"
bound=$(git cat-file -p "$node" | awk '$4=="args"{print $3}')
[ "$(git cat-file -p "$bound" | awk '$4=="x"{print $3}')" \
  = "$(git -C "$SRC" rev-parse "HEAD:payload")" ] \
  || fail "the bound arg is not the source repo's own tree:
$(git cat-file -p "$bound")"
echo "  ok: the ArgTree carries the foreign repo's oid, and no URL" >&2

echo "== the client fetched nothing: the fetch was the SERVER's ==" >&2
# The point of moving resolution. This repo has no remote for $REPO, no
# credential for it and no objects from it — it just named a tree by hash that
# only the server has ever held.
#
# THE COMMITS, AND ONLY THE COMMITS. A content tree is the wrong probe here and
# would contradict the invariant below: `payload` in the foreign repo is
# byte-identical to `./payload` here, so it IS the same oid, and this repo has
# it because it committed those bytes itself. Nothing was fetched to get it.
# A commit carries its author and parent, so the foreign repo's are unique to it.
[ "$(git rev-parse --is-shallow-repository)" = false ] \
  || fail "resolving a locator made the caller shallow"
for object in "$PARENT" "$SHA"; do
  if git cat-file -e "$object" 2>/dev/null; then
    fail "the client holds commit $object: it fetched the foreign repo itself"
  fi
done
echo "  ok: no commit of the foreign repo entered this client's store" >&2

args=(--base:@=DEEP-DEPS/bash --worker1:@=test/check.sh
      "--tree:@@=$REPO?rev=$SHA&dir=payload"
      "--file:@@=$REPO?rev=$SHA&dir=decoy.txt"
      "--whole:@@=$REPO?rev=$SHA"
      --local:@=payload
      "--repo=$REPO" "--sha=$SHA")

echo "== a locator resolves to the foreign repo's own oids ==" >&2
out=$("$CAOS_CLI" run "${args[@]}") || fail "the remote-ref run failed: $out"
got() { printf '%s\n' "$out" | awk -v k="$1" '$1==k{print $2}'; }

[ "$(got tree)" = "$(git -C "$SRC" rev-parse "HEAD:payload")" ] \
  || fail "dir= subtree resolved to $(got tree), not the source repo's payload tree"
[ "$(got file)" = "$(git -C "$SRC" rev-parse "HEAD:decoy.txt")" ] \
  || fail "dir= file resolved to $(got file), not the source repo's blob"
[ "$(got whole)" = "$(git -C "$SRC" rev-parse "HEAD^{tree}")" ] \
  || fail "a locator with no dir= resolved to $(got whole), not the commit's tree"
echo "  ok: subtree, blob and whole-tree all landed as the source repo's own oids" >&2

# THE INVARIANT. Same content, one named by URL+rev and one by a path in this
# repo — if the URL were anywhere in the key these could not be the same object.
[ "$(got tree)" = "$(got local)" ] \
  || fail "a remote ref and a local path over identical bytes disagreed: $(got tree) vs $(got local)"
printf '%s\n' "$out" | grep -q '^note from another repo$' \
  || fail "the worker could not read the fetched content: $out"
# A WORKER RESOLVES ONE TOO, and lands on the same oid. It has no repository and
# no remote; what it has is the server, which is now the one thing in the system
# that resolves a locator.
[ "$(got worker-tree)" = "$(got tree)" ] \
  || fail "a worker's resolve gave $(got worker-tree), the client's gave $(got tree)"
echo "  ok: identical to the local path arg, readable in the worker, and resolvable from one" >&2

echo "== a consumer repo pins caos by locator and runs its own worker ==" >&2
consumer=$("$CAOS_CLI" run "--base:@@=$REPO?rev=$SHA&dir=tool" --in:@=payload) \
  || fail "the consumer-story run failed: $consumer"
[ "$consumer" = "consumer worker saw: from another repo" ] \
  || fail "the consumer worker produced: $consumer"
echo "  ok: --base:@@= evaluated the foreign entry, which pinned caos back" >&2

echo "== the content-addressing rules are enforced, not advisory ==" >&2
refuses() { # <locator> <error fragment> <what was refused>
  if "$CAOS_CLI" curry --base:docker=unused "--x:@@=$1" >/dev/null 2>/tmp/err; then
    fail "accepted $3"
  fi
  grep -q -- "$2" /tmp/err || fail "wrong error for $3: $(cat /tmp/err)"
}
refuses "$REPO"                   "must pin a commit"  "a remote ref with no rev"
refuses "$REPO?ref=main"          "mutable"            "a branch instead of a commit"
refuses "$REPO?rev=abc123"        "full-length"        "a short rev"
refuses "$REPO?rev=$SHA&dir=nope" "\"nope\" not found"  "a dir= that isn't in the tree"
refuses "https://h/r?rev=$WS_SHA" "unknown scheme"     "a bare https url"
# A `path:` names a directory on the machine that WROTE the expression, and
# nothing resolves one any more. Refused BY NAME rather than reported as a
# missing path, which is what its author would otherwise be told about a
# directory sitting right there in front of them.
refuses "path:./payload"          "resolved by the caos server" "a host-directory locator"
echo "  ok: no rev, a mutable ref, a short rev, a missing dir, a bare URL, a path:" >&2

echo "== a pinned rev is a memo: re-resolving needs no remote at all ==" >&2
# The pin is a content hash, so the objects the first resolve fetched are the
# answer forever — recorded on the server as `refs/caos/locator-trees/<rev>`.
# Stopping the daemon and deleting the sources proves it: a resolve that reached
# for the network would now fail outright, and each `caos-cli` below is a FRESH
# process, so no in-memory memo is covering for the server's record.
stop_daemon
rm -rf "$SRCBASE"
again=$("$CAOS_CLI" run "${args[@]}") || fail "re-resolving without the remote failed: $again"
[ "$again" = "$out" ] || fail "re-resolving without the remote differed:
$again
vs
$out"
# And `git+caos://` — the scheme a dev session's rewritten pin uses — is answered
# from the server's own store by construction: this URL is not a reachable host
# at all, and resolving it still lands on the same tree.
caosurl=$("$CAOS_CLI" curry --base:docker=unused \
  "--x:@@=git+caos://not-a-host?rev=$SHA&dir=payload") \
  || fail "a git+caos:// locator was not answered from the server's own store"
caosbound=$(git cat-file -p "$caosurl" | awk '$4=="args"{print $3}')
[ "$(git cat-file -p "$caosbound" | awk '$4=="x"{print $3}')" = "$(got tree)" ] \
  || fail "git+caos:// resolved to a different tree than the fetch did"
echo "  ok: resolved from the server's record with the source repos deleted" >&2
