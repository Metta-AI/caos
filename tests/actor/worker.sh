#!/bin/bash
# Actor wrapper + reference kv inner, in stages (a worker cannot block on a run,
# so each stage tail-calls the next with run-request-then):
#   start       put a=1             -> one commit, state/a == 1
#   after-put   get a               -> reply 1, head unchanged (a read commits nothing)
#   after-get   put a=1 again       -> head unchanged (same state, no commit, no push).
#                                      This is also the crash-after-push case: a retry
#                                      re-applies the message and reaches the same head.
#   after-idem  put b=2             -> a second commit whose parent is the first
#   after-b     fresh branch, impure inner pushes a competing commit mid-request
#   raced       the request FAILED (lost race, not cached); the competing head stands;
#               send the identical request again
#   retried     the retry succeeded on top of the winner: state has x (winner) and a
#   after-conc  12 concurrent puts (map-then, retried on a lost race) all landed
#   after-lazy  a read touched one entry and the inner saw the others unmaterialized
#   after-hit1/after-hit2
#               the same read twice with different nonces ran the inner once
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
    --bash:@=/cas/args/bash --actor:@=/cas/args/actor --kv:@=/cas/args/kv \
    --probe:@=/cas/args/probe --mapper:@=/cas/args/mapper \
    --state-ref="$STATE_REF" "$@"
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

kv_inner() {
  caos curry --base:@=/cas/args/bash --worker1:@=/cas/args/kv
}

# probe_inner [--race-ref=R] [--count-ref=C]: the impure inner, in this image.
probe_inner() {
  caos curry --base:@=/cas/args/base --worker1:@=/cas/args/probe \
    --kv:@=/cas/args/kv "$@"
}

# actor_request <message> <nonce> <inner>: the complete request for one message.
actor_request() {
  local message=$1 nonce=$2 inner=$3
  printf '%s\n' "$message" > /tmp/msg
  rm -f /cas/msg
  caos put /tmp/msg /cas/msg > /dev/null || fail "staging the message"
  caos prepare-request --base:@=/cas/args/actor --state-ref="$STATE_REF" \
    --inner:hash="$inner" --nonce="$nonce-$SALT" --message:@=/cas/msg
}

# call <message> <nonce> <inner> <next stage> [next args...]; CATCH=1 delivers a
# failed request to the next stage as --error instead of failing the test.
call() {
  local message=$1 nonce=$2 inner=$3 next_stage=$4 request catch=()
  shift 4
  request=$(actor_request "$message" "$nonce" "$inner") || fail "preparing '$message'"
  if [ "${CATCH:-}" = 1 ]; then catch=(--catch); fi
  caos run-request-then "$request" --then:hash="$(next "$next_stage" "$@")" "${catch[@]}"
}

fresh_ref() { printf 'refs/heads/actors/test-%s-%s-%s-%s' "$1" "$(date +%s%N)" "$$" "$RANDOM"; }

read_arg() { caos get "/cas/args/$1" || fail "reading --$1"; cat "/cas/args/$1"; }

if [ "$stage" = start ]; then
  STATE_REF=$(fresh_ref main)
else
  STATE_REF=$(read_arg state-ref)
fi

case "$stage" in
start)
  if remote_head "$STATE_REF" > /dev/null; then fail "fresh ref already exists"; fi
  call "put a 1" n1 "$(kv_inner)" after-put
  ;;

after-put)
  h1=$(remote_head "$STATE_REF") || fail "put created no branch"
  [ "$(state_file "$h1" a)" = 1 ] || fail "state/a is not 1"
  [ "$(git rev-list --count "$h1")" = 1 ] || fail "first update is not a root commit"
  call "get a" n2 "$(kv_inner)" after-get --h1="$h1"
  ;;

after-get)
  h1=$(read_arg h1)
  [ "$(remote_head "$STATE_REF")" = "$h1" ] || fail "a read changed the head"
  caos get /cas/args/result || fail "reading the reply"
  [ "$(cat /cas/args/result)" = 1 ] || fail "get a replied '$(cat /cas/args/result)'"
  call "put a 1" n3 "$(kv_inner)" after-idem --h1="$h1"
  ;;

after-idem)
  h1=$(read_arg h1)
  [ "$(remote_head "$STATE_REF")" = "$h1" ] || fail "an unchanged put made a commit"
  call "put b 2" n4 "$(kv_inner)" after-b --h1="$h1"
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
  # A forced lost race: on a fresh branch the impure inner pushes a competing
  # commit while the request is in flight, so the wrapper's lease (no head) fails.
  STATE_REF=$(fresh_ref race)
  CATCH=1 call "put a 1" n5 "$(probe_inner --race-ref="$STATE_REF")" raced
  ;;

raced)
  caos get /cas/args/error 2>/dev/null || fail "the raced request did not fail (--error missing)"
  winner=$(remote_head "$STATE_REF") || fail "the competing writer left no branch"
  [ "$(state_file "$winner" x)" = 0 ] || fail "the head is not the competing commit"
  git cat-file -e "$winner:state/a" 2>/dev/null && fail "the lost request published anyway"
  # The identical request again (same nonce): a cached failure would replay the
  # failure; instead it re-runs against the new head and succeeds.
  call "put a 1" n5 "$(probe_inner --race-ref="$STATE_REF")" retried --winner="$winner"
  ;;

retried)
  winner=$(read_arg winner)
  head=$(remote_head "$STATE_REF") || fail "branch vanished"
  [ "$head" != "$winner" ] || fail "the retry published nothing"
  git fetch -q caos "$head" || fail "fetching $head"
  [ "$(git rev-parse "$head^1")" = "$winner" ] || fail "the retry is not on top of the winner"
  [ "$(state_file "$head" x)" = 0 ] || fail "the winner's entry was lost"
  [ "$(state_file "$head" a)" = 1 ] || fail "state/a missing after the retry"
  # Concurrent writers, each retrying a lost race, must converge with no lost update.
  STATE_REF=$(fresh_ref conc)
  rm -rf /tmp/msgs
  mkdir -p /tmp/msgs
  for n in 1 2 3 4 5 6 7 8 9 10 11 12; do printf 'put c%s v%s\n' "$n" "$n" > "/tmp/msgs/m$n"; done
  caos put /tmp/msgs /cas/msgs > /dev/null || fail "staging the messages"
  mapper=$(caos curry --base:@=/cas/args/base --worker1:@=/cas/args/mapper \
    --bash:@=/cas/args/bash --actor:@=/cas/args/actor --kv:@=/cas/args/kv \
    --state-ref="$STATE_REF" --test-salt:@=/cas/args/test-salt)
  caos map-then /cas/msgs --map:hash="$mapper" --then:hash="$(next after-conc)"
  ;;

after-conc)
  head=$(remote_head "$STATE_REF") || fail "no branch after the concurrent puts"
  for n in 1 2 3 4 5 6 7 8 9 10 11 12; do
    [ "$(state_file "$head" "c$n")" = "v$n" ] || fail "update c$n was lost"
  done
  [ "$(git rev-list --count "$head")" = 12 ] || fail "expected a linear chain of 12 commits"
  call "getcheck c3" n6 "$(kv_inner)" after-lazy --h="$head"
  ;;

after-lazy)
  head=$(read_arg h)
  [ "$(remote_head "$STATE_REF")" = "$head" ] || fail "a read changed the head"
  caos get /cas/args/result || fail "reading the reply"
  [ "$(cat /cas/args/result)" = v3 ] || fail "getcheck replied '$(cat /cas/args/result)'"
  # The same read twice, different nonces: the inner (pure, so cached) runs once.
  count_ref="refs/heads/actors-count/$(date +%s%N)-$$-$RANDOM"
  call "get c4" n7 "$(probe_inner --count-ref="$count_ref")" after-hit1 --count-ref="$count_ref"
  ;;

after-hit1)
  count_ref=$(read_arg count-ref)
  remote_head "$count_ref" > /dev/null || fail "the inner did not run"
  call "get c4" n8 "$(probe_inner --count-ref="$count_ref")" after-hit2 --count-ref="$count_ref"
  ;;

after-hit2)
  count_ref=$(read_arg count-ref)
  last=$(remote_head "$count_ref") || fail "count ref vanished"
  git fetch -q caos "$last" || fail "fetching $last"
  [ "$(git rev-list --count "$last")" = 1 ] \
    || fail "the inner ran $(git rev-list --count "$last") times; the repeat should hit the cache"
  printf 'actor: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
