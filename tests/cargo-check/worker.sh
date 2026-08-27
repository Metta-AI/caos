#!/bin/bash
# tests/cargo-check — a WORKER test: no client, no repo.
#
# Exercises the cargo worker (worker-cargo, design/cargo-workers.md): a
# whole-workspace `cargo check/test` over a source tree, `--offline` atop the
# image's baked toolchain + deps. Asserts: a passing `test` run reports its
# result as a value ({exit, stdout, stderr}); a compile error is likewise a
# VALUE (nonzero exit, diagnostics on stderr), never a run error; and an
# identical tree re-run returns the identical (cached) value.
#
# The mini packages here have no dependencies, so they exercise the worker's
# materialize-and-run path without touching the baked caos deps; the full
# dogfood (cargo check of the caos workspace itself) is tests/cargo-self.
#
# FOUR STAGES: a run cannot be waited on, so each assertion is the `then` of
# the run it is about.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
# EVERYTHING A LATER STAGE READS IS FORWARDED. `/cas/args/base` is the reserved
# base entry — the bash image — not this job's ArgTree, so currying onto it
# carries none of what the expression bound alongside it. --test-salt rides in
# every stage for the same reason: bound only at the top, a fresh value would
# re-run the first container and hit the memo for the rest.
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --cargo:@=/cas/args/cargo --mini:@=/cas/args/mini --broken:@=/cas/args/broken "$@"; }

# Every test runs in the Linux stack, so build musl (statics run there) — the
# system's one target. No host build has a consumer.
tgt="$(uname -m)-unknown-linux-musl"
cargo_job() { # <cmd> -> the ArgTree to run; the TREE rides as the subject
  caos curry --base:@=/cas/args/cargo --cmd="$1" "--target=$tgt"
}

case "$stage" in

start)
  echo "== cargo test: a passing package ==" >&2
  caos run-then /cas/args/mini --run:hash="$(cargo_job test)" --then:hash="$(next passed)"
  ;;

passed)
  caos get -r /cas/args/result || fail "reading the result"
  [ "$(cat /cas/args/result/exit)" = "0" ] \
    || fail "test: exit $(cat /cas/args/result/exit); stderr: $(cat /cas/args/result/stderr)"
  grep -q "test result: ok. 1 passed" /cas/args/result/stdout \
    || fail "no passing test output: $(cat /cas/args/result/stdout)"
  echo "  ok: tests ran and passed" >&2

  echo "== cargo check: a compile error is a value, not a run error ==" >&2
  caos run-then /cas/args/broken --run:hash="$(cargo_job check)" \
    --then:hash="$(next broken --mini-result="$(caos hash /cas/args/result)")"
  ;;

broken)
  caos get -r /cas/args/result || fail "reading the result"
  [ "$(cat /cas/args/result/exit)" != "0" ] || fail "broken check exited 0"
  grep -q "mismatched types" /cas/args/result/stderr \
    || fail "no diagnostics: $(cat /cas/args/result/stderr)"
  echo "  ok: diagnostics surfaced, exit $(cat /cas/args/result/exit)" >&2

  echo "== identical tree: the cached value comes back ==" >&2
  caos get /cas/args/mini-result
  caos run-then /cas/args/mini --run:hash="$(cargo_job test)" \
    --then:hash="$(next cached --mini-result="$(cat /cas/args/mini-result)")"
  ;;

cached)
  # By OID, not by diffing two checkouts: a result is content-addressed, so an
  # equal hash is a stronger statement than equal exit and stdout, and it needs
  # nothing carried but the hash.
  caos get /cas/args/mini-result
  want=$(cat /cas/args/mini-result)
  got=$(caos hash /cas/args/result)
  [ "$got" = "$want" ] || fail "re-run of an identical tree differed: $got vs $want"
  echo "  ok: identical result" >&2

  printf 'cargo-check: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
