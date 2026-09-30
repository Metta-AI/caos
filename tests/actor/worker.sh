#!/bin/bash
# Actor wrapper + reference kv inner, in stages (a worker cannot block on a run,
# so each stage tail-calls the next with run-request-then):
#   start       put a=1             -> one commit, state/a == 1
#   after-put   get a               -> reply 1, head unchanged (a read commits nothing)
#   after-get   put a=1 again       -> head unchanged (same state, no commit, no push)
#   after-idem  put b=2             -> a second commit whose parent is the first
#   after-b     verify the chain and state, report
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi

caos get /cas/args/test-salt || fail "reading --test-salt"
SALT=$(cat /cas/args/test-salt)

: "${CAOS_SERVER_URL:?this test needs CAOS_SERVER_URL from the runner}"
rm -rf /tmp/repo
mkdir -p /tmp/repo
cd /tmp/repo
git init -q .
git config user.email test@caos
git config user.name caos
git config gc.auto 0
git remote add caos "$CAOS_SERVER_URL"

next() {
  local next_stage=$1
  shift
  caos curry --base:@=/cas/args/base --worker1:@=/cas/args/worker1 \
    --stage="$next_stage" --test-salt:@=/cas/args/test-salt \
    --bash:@=/cas/args/bash --actor:@=/cas/args/actor --kv:@=/cas/args/kv "$@"
}

remote_head() {
  local line
  line=$(git ls-remote --refs caos "$1") || return 1
  [ -n "$line" ] || return 1
  printf '%s\n' "${line%%[[:space:]]*}"
}

state_file() { # <commit> <name>
  git fetch -q caos "$1" || fail "fetching $1"
  git show "$1:state/$2"
}

# actor_request <message> <nonce>: the complete request for one message.
actor_request() {
  local message=$1 nonce=$2 inner
  printf '%s\n' "$message" > /tmp/msg
  rm -f /cas/msg
  caos put /tmp/msg /cas/msg > /dev/null || fail "staging the message"
  inner=$(caos curry --base:@=/cas/args/bash --worker1:@=/cas/args/kv) \
    || fail "currying the inner"
  caos prepare-request --base:@=/cas/args/actor --state-ref="$STATE_REF" \
    --inner:hash="$inner" --nonce="$nonce-$SALT" --message:@=/cas/msg
}

call() { # <message> <nonce> <next stage> [next args...]
  local message=$1 nonce=$2 next_stage=$3 request
  shift 3
  request=$(actor_request "$message" "$nonce") || fail "preparing '$message'"
  caos run-request-then "$request" --then:hash="$(next "$next_stage" \
    --state-ref="$STATE_REF" "$@")"
}

if [ "$stage" = start ]; then
  STATE_REF="refs/heads/actors/test-$(date +%s%N)-$$-$RANDOM"
else
  caos get /cas/args/state-ref || fail "reading --state-ref"
  STATE_REF=$(cat /cas/args/state-ref)
fi

read_arg() { caos get "/cas/args/$1" || fail "reading --$1"; cat "/cas/args/$1"; }

case "$stage" in
start)
  if remote_head "$STATE_REF" > /dev/null; then fail "fresh ref already exists"; fi
  call "put a 1" n1 after-put
  ;;

after-put)
  h1=$(remote_head "$STATE_REF") || fail "put created no branch"
  [ "$(state_file "$h1" a)" = 1 ] || fail "state/a is not 1"
  [ "$(git rev-list --count "$h1")" = 1 ] || fail "first update is not a root commit"
  call "get a" n2 after-get --h1="$h1"
  ;;

after-get)
  h1=$(read_arg h1)
  [ "$(remote_head "$STATE_REF")" = "$h1" ] || fail "a read changed the head"
  call "put a 1" n3 after-idem --h1="$h1"
  ;;

after-idem)
  h1=$(read_arg h1)
  [ "$(remote_head "$STATE_REF")" = "$h1" ] || fail "an unchanged put made a commit"
  call "put b 2" n4 after-b --h1="$h1"
  ;;

after-b)
  h1=$(read_arg h1)
  h2=$(remote_head "$STATE_REF") || fail "branch vanished"
  [ "$h2" != "$h1" ] || fail "put b made no commit"
  git fetch -q caos "$h2" || fail "fetching $h2"
  [ "$(git rev-parse "$h2^1")" = "$h1" ] || fail "second update is not on the first"
  [ "$(git rev-list --count "$h2")" = 2 ] || fail "history is not a linear chain of two"
  [ "$(state_file "$h2" a)" = 1 ] || fail "state/a lost"
  [ "$(state_file "$h2" b)" = 2 ] || fail "state/b missing"
  printf 'actor: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
