#!/bin/bash
# tests/eval-then — a WORKER test: no client, no repo.
#
# Exercises `caos eval-path-then` (design/caos-expr.md): the SERVER-side
# evaluation continuation. A worker may not block on a run, so to evaluate a
# `.caos-expr` it records `{in, eval, then?}` and exits; the server walks the
# expression on a request thread — its own `run`s dispatched normally — and
# threads the result into `then`.
#
# THE CLAIM: evaluating a `run`-valued expression IS running that request. So
# the test evaluates `pkg`, and separately forms the very request that
# expression denotes (`prepare-request` over the same base, worker and arg) and
# runs it by identity — and the two must be the same object. That is identity
# between the evaluator and the dispatcher, asserted with nothing outside a
# worker. `--catch` is covered alongside it: a broken walk becomes a value the
# `then` receives instead of failing the request.
#
# THE PACKAGE NAMES ITS IMAGE BY OID (`--base:hash=`), not by a copied-in entry
# directory. An entry would be a SOURCE tree, so evaluating it would run the
# flake-builder — an image build, in the middle of a test about evaluation, and
# one that only stays cheap while the copy happens to hash identically to the
# real entry. `/cas/args/base` is the image this worker is already running in,
# so the oid is free and the walk dispatches exactly one run: the one under test.
#
# THE FIXTURE IS BUILT AND PUBLISHED, not committed: a worker has no git, and
# `caos put` is how it makes a tree nameable.
#
# FOUR STAGES: no run can be waited on, so each assertion is the `then` of the
# evaluation it is about.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --build:@=/cas/args/build "$@"; }

# A tree carrying `pkg`, whose `.caos-expr` builds its own directory by running
# build.sh in this image — a `run`-valued expression, so evaluating it
# DISPATCHES a sub-run, which is the whole point of server-side eval. Rebuilt by
# each stage that needs it, so every input is a stated function of this
# function rather than of the stages before it.
fixture() {
  local r=/tmp/tree
  rm -rf "$r"; mkdir -p "$r/pkg"
  caos get /cas/args/build || fail "reading build.sh"
  cp /cas/args/build "$r/pkg/build.sh"
  echo world > "$r/pkg/name"
  printf 'run --base:hash=%s --worker1:@=build.sh --name:@=name\n' \
    "$(caos hash /cas/args/base)" > "$r/pkg/.caos-expr"
  caos put "$r" /cas/fixture || fail "publishing the fixture"
}

case "$stage" in

start)
  echo "== a run-valued expression, evaluated server-side ==" >&2
  fixture
  caos eval-path-then /cas/fixture --eval=pkg --then:hash="$(next evaluated)"
  ;;

evaluated)
  caos get -r /cas/args/result || fail "reading the evaluated result"
  [ -f /cas/args/result/greeting ] || fail "the evaluated package has no greeting"
  [ "$(cat /cas/args/result/greeting)" = "hello world" ] \
    || fail "wrong greeting: $(cat /cas/args/result/greeting)"
  echo "  ok: the expression's run was dispatched and its result returned" >&2

  echo "== ...and it is the SAME OBJECT the request itself produces ==" >&2
  # `prepare-request` forms the ArgTree the expression denotes — same base, same
  # worker, same arg — and `run-request-then` runs THAT ONE, by identity. Handed
  # a request hash rather than a curry node, which is the verb's whole purpose.
  fixture
  caos get -r /cas/fixture || fail "reading the fixture back"
  req=$(caos prepare-request --base:hash="$(caos hash /cas/args/base)" \
    --worker1:@=/cas/fixture/pkg/build.sh --name:@=/cas/fixture/pkg/name) \
    || fail "forming the equivalent request"
  caos run-request-then "$req" \
    --then:hash="$(next direct --evaled="$(caos hash /cas/args/result)")"
  ;;

direct)
  caos get /cas/args/evaled || fail "reading --evaled"
  got=$(caos hash /cas/args/result)
  [ "$got" = "$(cat /cas/args/evaled)" ] \
    || fail "eval gave $(cat /cas/args/evaled), the request gave $got"
  echo "  ok: evaluating the expression == running the request ($got)" >&2

  echo "== --catch turns a broken walk into a value the then receives ==" >&2
  # A path that does not exist makes the walk fail. Without --catch this would
  # fail the request; with it the failure arrives as --error.
  fixture
  caos eval-path-then /cas/fixture --eval=no-such-dir \
    --then:hash="$(next caught)" --catch
  ;;

caught)
  [ -e /cas/args/error ] || fail "expected the broken walk to fail, but it succeeded"
  caos get /cas/args/error
  [ -s /cas/args/error ] || fail "the caught failure carried no text"
  echo "  caught: $(cat /cas/args/error)" >&2
  echo "  ok: a failed walk was delivered as --error and the request succeeded" >&2

  printf 'eval-then: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
