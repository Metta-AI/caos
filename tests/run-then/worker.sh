#!/bin/bash
# tests/run-then — a WORKER test: no client, no repo.
#
# Exercises run-then — the single-valued map-then (the continuation
# `{in, run?, then?, catch?}`): a plain tail call (--run only), the sub-run's
# result threading into `then` as --result, a nested promise from the run
# position, client-side flag validation, run-cycle detection, and `--catch` — a
# failing run delivered to `then` as --error instead of failing the request,
# with the uncaught case asserted alongside it so the default cannot drift.
#
# EIGHT STAGES: no run can be waited on, so each assertion is the `then` of the
# run it is about. The timings the old version printed are gone with the
# blocking calls that made them meaningful.
#
# THE NUMBER IS `--num`, NOT `--in`. `in` is not ours to bind: dev/run-test
# launches a test with `run-then <the test's tree> --run:hash=<the test>`, and
# the continuation's own `in` is that tree — so a test that binds `--in` is
# handed a directory instead, and dies at the first `cat` with "Is a directory".
# The fixtures below DO read `/cas/args/in`, correctly: each is dispatched by a
# `run-then /cas/args/num`, which is what binds their `in`.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --bash:@=/cas/args/bash --num:@=/cas/args/num --double:@=/cas/args/double \
  --combine:@=/cas/args/combine --driver:@=/cas/args/driver \
  --outer:@=/cas/args/outer --checks:@=/cas/args/checks --boom:@=/cas/args/boom \
  --catcher:@=/cas/args/catcher --cycle:@=/cas/args/cycle "$@"; }

# A bash worker running one of this test's fixture scripts.
fixture() { local w=$1; shift; caos curry --base:@=/cas/args/bash --worker1:@="/cas/args/$w" "$@"; }
result_text() { caos get /cas/args/result >/dev/null; cat /cas/args/result; }

case "$stage" in

start)
  # double.sh writes 2*<in>; combine.sh writes "in=<in> result=<result>".
  # driver.sh run-thens over --in with whatever run-img/then-img it was curried.
  echo "== run with no then: a plain tail call to run ==" >&2
  caos run-then /cas/args/num \
    --run:hash="$(fixture driver --run-img="$(fixture double)")" \
    --then:hash="$(next tail)"
  ;;

tail)
  [ "$(result_text)" = "42" ] || fail "expected 42, got: $(result_text)"
  echo "  ok: run(--in=21) -> 42 is the request's result" >&2

  echo "== run + then: the result threads into then as --result ==" >&2
  caos run-then /cas/args/num \
    --run:hash="$(fixture driver --run-img="$(fixture double)" --then-img="$(fixture combine)")" \
    --then:hash="$(next threaded)"
  ;;

threaded)
  [ "$(result_text)" = "in=21 result=42" ] \
    || fail "expected 'in=21 result=42', got: $(result_text)"
  echo "  ok: then saw --in=21 and --result=42" >&2

  echo "== an identical request is a cache hit with the same value ==" >&2
  caos run-then /cas/args/num \
    --run:hash="$(fixture driver --run-img="$(fixture double)" --then-img="$(fixture combine)")" \
    --then:hash="$(next cached)"
  ;;

cached)
  [ "$(result_text)" = "in=21 result=42" ] || fail "cached rerun differs: $(result_text)"
  echo "  ok: rerun -> same value" >&2

  echo "== a nested promise from the run position resolves ==" >&2
  # outer.sh's whole body is itself a run-then (over the curried double), so the
  # driver's `run` sub-run returns a promise the server must collapse before
  # combine sees --result.
  caos run-then /cas/args/num \
    --run:hash="$(fixture driver \
      --run-img="$(fixture outer --inner-img="$(fixture double)")" \
      --then-img="$(fixture combine)")" \
    --then:hash="$(next nested)"
  ;;

nested)
  [ "$(result_text)" = "in=21 result=42" ] \
    || fail "nested promise: expected 'in=21 result=42', got: $(result_text)"
  echo "  ok: run's promise collapsed to 42 before then" >&2

  echo "== --map/--run exclusivity and missing --run are rejected ==" >&2
  caos run-then /cas/args/num --run:hash="$(fixture checks)" --then:hash="$(next validated)"
  ;;

validated)
  [ "$(result_text)" = "ok" ] || fail "checks.sh did not pass: $(result_text)"
  echo "  ok: bad flag combinations refused before anything is recorded" >&2

  echo "== without --catch, a failing run fails the whole request ==" >&2
  # The default, asserted so `--catch` below cannot quietly become the behaviour
  # everywhere: a pipeline that loses a step has no business reporting success.
  # The `--catch` HERE is on our own dispatch of the uncaught driver, so the
  # propagation arrives as a value; the driver itself catches nothing.
  caos run-then /cas/args/num \
    --run:hash="$(fixture driver --run-img="$(fixture boom)" --then-img="$(fixture catcher)")" \
    --then:hash="$(next uncaught)" --catch
  ;;

uncaught)
  [ -e /cas/args/error ] || fail "expected the failing run to fail the request"
  caos get /cas/args/error
  grep -q "exit status: 1" /cas/args/error \
    || fail "no worker failure reported; got: $(cat /cas/args/error)"
  echo "  ok: the run's failure propagated" >&2

  echo "== with --catch, the failure reaches then as --error ==" >&2
  # Same failing run, same then — only the flag on the DRIVER differs, so what
  # changes is the interpreter's handling and nothing else.
  caos run-then /cas/args/num \
    --run:hash="$(fixture driver --run-img="$(fixture boom)" \
      --then-img="$(fixture catcher)" --catch=1)" \
    --then:hash="$(next caught)"
  ;;

caught)
  [ "$(result_text)" = "in=21 caught=yes" ] \
    || fail "expected 'in=21 caught=yes', got: $(result_text)"
  echo "  ok: then ran with --in and --error, and the request succeeded" >&2

  echo "== a run-then cycle is detected (by the server) ==" >&2
  # cycle.sh re-curries itself (content-addressed, so the sub-request is
  # byte-identical to the in-flight one) and run-thens the same input.
  caos run-then /cas/args/num --run:hash="$(fixture cycle)" \
    --then:hash="$(next cycled)" --catch
  ;;

cycled)
  [ -e /cas/args/error ] || fail "expected the self-recursive run-then to fail"
  caos get /cas/args/error
  grep -q "run cycle detected" /cas/args/error \
    || fail "no cycle reported; got: $(cat /cas/args/error)"
  echo "  ok: run failed with a run-cycle error" >&2

  printf 'run-then: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
