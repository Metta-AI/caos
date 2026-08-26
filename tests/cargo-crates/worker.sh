#!/bin/bash
# tests/cargo-crates — a WORKER test: no client, no repo.
#
# Per-crate decomposition (mode=all, design/cargo-workers.md) over a two-crate
# workspace where b depends on a. Asserts: a clean check and a clean test; a
# broken dependency propagating to its dependent AS A VALUE, with both crates'
# sections and the real diagnostics; an edit confined to the dependent; and an
# identical tree served from cache.
#
# THE FIXTURE EDITS ARE APPLIED FROM PRISTINE, not accumulated. Each stage
# starts from the `ws` it was handed and applies exactly the edits its own run
# needs, so every run's input is a stated function of the fixture rather than of
# everything that happened before it. That also makes the "fix a" run honest
# about itself: fixing a returns the tree to pristine, so it has the same key as
# the first check and IS a cache hit — which was true before and invisible.
#
# Bash parameter expansion, not sed: std/bash carries no sed (CLAUDE.md), and
# these are literal substitutions.
#
# The timings the old version printed are gone with the blocking calls that made
# them meaningful — each run is a container round trip now.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --cargo:@=/cas/args/cargo --ws:@=/cas/args/ws "$@"; }

tgt="$(uname -m)-unknown-linux-musl"
cargo_job() { caos curry --base:@=/cas/args/cargo --cmd="$1" --mode=all "--target=$tgt"; }

# A writable copy of the fixture, optionally with one edit applied, published so
# it can be a run's subject. `$1` is "" (pristine), "break-a" or "edit-b".
staged_ws() { # <edit> -> a /cas path
  rm -rf /tmp/ws && mkdir -p /tmp/ws
  caos get -r /cas/args/ws || fail "reading the fixture"
  cp -rL /cas/args/ws/. /tmp/ws/ && chmod -R u+w /tmp/ws
  case "$1" in
    break-a) local f=/tmp/ws/a/src/lib.rs c; c=$(cat "$f"); printf '%s' "${c//x \* 2/x * \"two\"}" > "$f" ;;
    edit-b)  local f=/tmp/ws/b/src/main.rs c; c=$(cat "$f"); printf '%s' "${c//b says/b announces}" > "$f" ;;
  esac
  caos put /tmp/ws /cas/staged-ws || fail "publishing the staged workspace"
  echo /cas/staged-ws
}

case "$stage" in

start)
  echo "== mode=all check: a clean two-crate workspace ==" >&2
  caos run-then "$(staged_ws '')" --run:hash="$(cargo_job check)" --then:hash="$(next checked)"
  ;;

checked)
  caos get -r /cas/args/result || fail "reading the result"
  [ "$(cat /cas/args/result/exit)" = "0" ] \
    || fail "check: exit $(cat /cas/args/result/exit); stderr: $(cat /cas/args/result/stderr)"
  echo "  ok: clean check" >&2

  echo "== mode=all test: b's unit test runs ==" >&2
  caos run-then "$(staged_ws '')" --run:hash="$(cargo_job test)" --then:hash="$(next tested)"
  ;;

tested)
  caos get -r /cas/args/result || fail "reading the result"
  [ "$(cat /cas/args/result/exit)" = "0" ] \
    || fail "test: exit $(cat /cas/args/result/exit); stderr: $(cat /cas/args/result/stderr)"
  grep -q "test result: ok. 1 passed" /cas/args/result/stdout \
    || fail "b's test didn't run: $(cat /cas/args/result/stdout)"
  echo "  ok: tests ran" >&2

  echo "== a broken dep propagates to its dependent as a value ==" >&2
  caos run-then "$(staged_ws break-a)" --run:hash="$(cargo_job check)" --then:hash="$(next broken)"
  ;;

broken)
  caos get -r /cas/args/result || fail "reading the result"
  [ "$(cat /cas/args/result/exit)" != "0" ] || fail "broken dep: exit 0"
  grep -q "── a ──" /cas/args/result/stderr || fail "no a section"
  grep -q "── b ──" /cas/args/result/stderr || fail "no b section (propagation)"
  # b's section carries a's diagnostics — the failure bubbled as a value.
  grep -q "cannot multiply" /cas/args/result/stderr || fail "no diagnostics"
  echo "  ok: dep failure propagated with diagnostics" >&2

  # `fix a` IS the pristine tree — nothing to apply.
  echo "== fix; edit only b; a's jobs are cache hits ==" >&2
  caos run-then "$(staged_ws '')" --run:hash="$(cargo_job check)" --then:hash="$(next fixed)"
  ;;

fixed)
  caos get -r /cas/args/result || fail "reading the result"
  [ "$(cat /cas/args/result/exit)" = "0" ] || fail "fixed check failed"
  caos run-then "$(staged_ws edit-b)" --run:hash="$(cargo_job check)" --then:hash="$(next bedit)"
  ;;

bedit)
  caos get -r /cas/args/result || fail "reading the result"
  [ "$(cat /cas/args/result/exit)" = "0" ] || fail "b-edit check failed"
  echo "  ok: b-only edit checked" >&2

  echo "== identical tree: the cached value comes back ==" >&2
  printf '%s\n' "$(caos hash /cas/args/result)" > /tmp/bedit
  caos put /tmp/bedit /cas/bedit
  caos run-then "$(staged_ws edit-b)" --run:hash="$(cargo_job check)" \
    --then:hash="$(next cached --bedit:@=/cas/bedit)"
  ;;

cached)
  caos get /cas/args/bedit
  [ "$(caos hash /cas/args/result)" = "$(cat /cas/args/bedit)" ] \
    || fail "cached rerun differed"
  echo "  ok: cached" >&2
  printf 'cargo-crates: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
