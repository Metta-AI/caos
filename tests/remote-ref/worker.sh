#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI set,
# INSIDE the dev stack — the suite's per-test job (tests/lib stages the repo, then runs this).
#
# `--name:@@=<git ref>`: a tree that lives in ANOTHER repo, pinned by a commit
# sha (design/flake-inputs.md). The claim under test is that the locator is a
# FETCH COORDINATE AND NOTHING ELSE — the client resolves url+rev to an oid at
# eval time, and what enters the ArgTree (so the cache key) is that oid,
# byte-for-byte what a local `:@=` of the same content would have produced. Every
# assertion below is a way of pinning that down: same oid as the foreign repo's
# own, same oid as the local path, and still resolvable once the remote is gone.
#
# `git+file://` rather than a network host, deliberately: this test is about
# resolution, not about anyone's TLS or uptime. The one thing a real host adds is
# `uploadpack.allowReachableSHA1InWant` — GitHub sets it, which is exactly why
# the locator pins a COMMIT and selects within it with `dir=` instead of naming a
# subtree hash — so the fixture sets it too and the fetch is the same shape it
# would be against github.com.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

# ---- the local half of the comparison ---------------------------------------
mkdir -p payload
printf 'from another repo\n' > payload/note.txt
commit remote-ref-ws
WS_SHA=$(git rev-parse HEAD)

# ---- the foreign repo, which is all a `:@@=` ever names ----------------------
SRC=/tmp/remote-ref-src
rm -rf "$SRC"; mkdir -p "$SRC/payload" "$SRC/tool"
git init -q "$SRC"
git -C "$SRC" config uploadpack.allowReachableSHA1InWant true
# The same BYTES as ./payload, reached the other way. Two files and a subtree, so
# the run below covers a subtree, a blob and the whole root in one container.
printf 'from another repo\n' > "$SRC/payload/note.txt"
printf 'not this one\n' > "$SRC/decoy.txt"

# THE CONSUMER STORY, in miniature: a repo that is NOT caos pins caos by locator
# and curries its own worker onto caos' std/bash. Nothing of caos is committed
# here — the pin is the whole dependency. (It pins this workspace, whose commit
# the client already holds; that is not a shortcut but the point of pinning by
# content — see the memo assertion at the end.)
cat > "$SRC/tool/.caos-expr" <<EXPR
# A consumer's entry: caos' std/bash by pinned locator, its own script curried on.
curry --base:@@=git+file://$PWD?rev=$WS_SHA&dir=DEEP-DEPS/bash --worker1:@=run.sh
EXPR
cat > "$SRC/tool/run.sh" <<'RUN'
#!/bin/bash
set -euo pipefail
caos get -r /cas/args/in
printf 'consumer worker saw: %s\n' "$(cat /cas/args/in/note.txt)" > /tmp/out
caos put /tmp/out /cas/out
RUN
git -C "$SRC" add -A
git -C "$SRC" -c user.email=test@caos -c user.name=caos commit -qm remote-ref-src
SHA=$(git -C "$SRC" rev-parse HEAD)
REPO="git+file://$SRC"

echo "== resolution happens client-side, with no compute at all ==" >&2
# The tightest possible statement of the claim, and it runs nothing: `curry`
# only BUILDS an ArgTree, so what this inspects is the arg entry itself.
node=$("$CAOS_CLI" curry --base:docker=unused "--x:@@=$REPO?rev=$SHA&dir=payload") \
  || fail "a client-side resolve failed"
bound=$(git cat-file -p "$node" | awk '$4=="args"{print $3}')
[ "$(git cat-file -p "$bound" | awk '$4=="x"{print $3}')" \
  = "$(git -C "$SRC" rev-parse "HEAD:payload")" ] \
  || fail "the bound arg is not the source repo's own tree:
$(git cat-file -p "$bound")"
echo "  ok: the ArgTree carries the foreign repo's oid, and no URL" >&2

args=(--base:@=DEEP-DEPS/bash --worker1:@=test/check.sh
      "--tree:@@=$REPO?rev=$SHA&dir=payload"
      "--file:@@=$REPO?rev=$SHA&dir=decoy.txt"
      "--whole:@@=$REPO?rev=$SHA"
      --local:@=payload)

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
printf '%s\n' "$out" | grep -q '^worker-refused ok$' \
  || fail "the worker-cannot-fetch assertion did not run: $out"
echo "  ok: identical to the local path arg, readable in the worker, refused inside one" >&2

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
echo "  ok: no rev, a mutable ref, a short rev, a missing dir and a bare URL" >&2

echo "== a pinned rev is a memo: re-resolving needs no remote at all ==" >&2
# The pin is a content hash, so the objects the first resolve fetched are the
# answer forever. Deleting the source repo proves it: a second resolve that
# reached for the network would now fail outright.
rm -rf "$SRC"
again=$("$CAOS_CLI" run "${args[@]}") || fail "re-resolving without the remote failed: $again"
[ "$again" = "$out" ] || fail "re-resolving without the remote differed:\n$again\nvs\n$out"
echo "  ok: resolved from local objects with the source repo deleted" >&2
