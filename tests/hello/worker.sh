#!/bin/bash
# tests/hello — a WORKER test: no client, no repo.
#
# `std/hello` is what a person runs to check that a caos installation works, so
# what this pins down is the PROPERTY that makes it useful for that: one call,
# the answer as a plain blob, nothing to go looking for. If the result ever
# became a tree, `run` would refuse to stream it and the entry would stop being
# a smoke test — that is the regression this catches.
#
# WHY IT IS NOT A CLIENT TEST, despite one claim being about `--base:@=` path
# resolution. `eval-path-then` walks a path the SAME way a client `eval-path`
# does — same crate, same descent through every `.caos-expr` on the way — which
# is exactly what tests/eval-then asserts. What is genuinely client-only is
# BLOCKING on the answer, and blocking was never the subject here.
#
# FIVE STAGES: no run can be waited on, so each assertion is the `then` of the
# run it is about.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi

# THE UNBUILT HELLO ENTRY, forwarded stage to stage. `--hello:@=DEEP-DEPS/hello`
# is the BUILT image (naming a dependency evaluates it), and one claim below
# needs the source entry instead — which is reachable, unevaluated, at
# `/cas/args/in/DEEP-DEPS/hello`, because dev/run-test runs a test by
# `run-then`ning the test's own deepened tree.
#
# ONLY THE FIRST JOB HAS `in`, though: a `then` receives `--in`/`--result` from
# ITS continuation, and these stages chain through `run-request-then`, which
# binds no `in` at all. So it is picked up once and re-bound by name, like
# everything else a later stage reads.
if [ -e /cas/args/hello-src ]; then
  HELLO_SRC=/cas/args/hello-src
else
  # ONE LEVEL AT A TIME: `caos get` expands a directory's immediate children,
  # so the tree has to be opened before `DEEP-DEPS` inside it can be.
  caos get /cas/args/in || fail "expanding this test's tree"
  caos get /cas/args/in/DEEP-DEPS || fail "expanding this test's deps"
  HELLO_SRC=/cas/args/in/DEEP-DEPS/hello
fi

next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --hello:@=/cas/args/hello --lift:@=/cas/args/lift \
  --hello-src:@="$HELLO_SRC" "$@"; }

# The hello image, by oid — what every run below is.
HELLO=$(caos hash /cas/args/hello)
# A run of hello with the given arguments, as a REQUEST — `prepare-request`,
# not a curry handed to `run-then`. A continuation binds its own `in`, and this
# worker's whole job is to report every argument it was given, so `run-then`
# made hello answer "3 arguments" with `in = blob … (6410 bytes)` among them.
# `run-request-then` runs an ArgTree by IDENTITY, so the request holds exactly
# what the caller wrote — which is the claim.
hello_run() { caos prepare-request --base:hash="$HELLO" "$@"; }
result_text() { caos get /cas/args/result >/dev/null; cat /cas/args/result; }
# Every assertion below is a whole-line match against the report.
has_line() { printf '%s\n' "$1" | grep -qx "$2"; }

case "$stage" in

start)
  echo "== hello streams its arguments back, with no output path ==" >&2
  caos run-request-then \
    "$(hello_run --greeting=hi --who=world)" \
    --then:hash="$(next mirrored)"
  ;;

mirrored)
  out=$(result_text)
  [ "$(printf '%s\n' "$out" | head -1)" = "hello: 2 arguments" ] \
    || fail "unexpected header: $out"
  has_line "$out" '  greeting = hi' || fail "greeting not mirrored: $out"
  has_line "$out" '  who = world'   || fail "who not mirrored: $out"
  echo "  ok: both literals came back verbatim" >&2

  echo "== self-reference is not mirrored as an argument ==" >&2
  # Two kinds ride in every ArgTree and neither is a caller's input: the
  # reserved entries (`base` is the worker's own IMAGE — describing it would
  # drag a whole image tree into a smoke test), and `workerN`, which for a
  # compiled std entry is the worker's own binary, bound by the curry that
  # makes it runnable. The first draft of that worker reported
  # `worker1 = blob … (5055368 bytes)` — having fetched all 5 MB to say so.
  for leaked in base salt secret-hash worker1; do
    if printf '%s\n' "$out" | grep -q "  $leaked = "; then
      fail "$leaked leaked into the report as an argument: $out"
    fi
  done
  echo "  ok: reserved entries and workerN stay out of the report" >&2

  echo "== a tree argument is summarized, not walked ==" >&2
  rm -rf /tmp/t; mkdir -p /tmp/t; printf 'x\n' > /tmp/t/a.txt
  caos put /tmp/t /cas/t || fail "publishing the tree argument"
  caos run-request-then \
    "$(hello_run --src:@=/cas/t)" \
    --then:hash="$(next treearg --want="$(caos hash /cas/t)")"
  ;;

treearg)
  caos get /cas/args/want || fail "reading --want"
  out=$(result_text)
  has_line "$out" "  src = tree $(cat /cas/args/want)" \
    || fail "a tree arg should report its kind and hash, got: $out"
  echo "  ok: reported as 'tree <oid>' — the hash the cache keyed on" >&2

  echo "== a base path that exists only in an EVALUATED tree resolves ==" >&2
  # `--base:@=` descends from the tree ROOT through every `.caos-expr` on the
  # way, rather than taking the named directory and evaluating it alone. The
  # difference is invisible unless an ANCESTOR of the path is itself evaluable,
  # so this builds that shape: `outer/` produces a tree whose `tool` entry is
  # the hello entry, and nothing is at `outer/tool` in the fixture.
  #
  # Under the old behaviour this could not work at all — `outer/tool` is not a
  # directory, so resolution stopped before evaluating anything. It is the same
  # asymmetry `:@@=` had (design/flake-inputs.md, 4a): an entry reachable
  # remotely but not locally.
  #
  # THE HELLO ENTRY GOES IN BY REFERENCE, unbuilt. `/cas/args/in` is this
  # test's own deepened tree, so `DEEP-DEPS/hello` there is the SOURCE entry
  # (binding it with `:@=` would have evaluated it), and `caos put` resolves the
  # symlink to its recorded hash — so the fixture's copy is the same object the
  # suite already built, not a near-copy that would rebuild.
  caos get /cas/args/lift || fail "reading lift.sh"
  r=/tmp/tree; rm -rf "$r"; mkdir -p "$r/outer/src"
  ln -s "$HELLO_SRC" "$r/outer/src/hello"
  cp /cas/args/lift "$r/outer/lift.sh"
  printf 'run --base:hash=%s --worker1:@=lift.sh --src:@=src\n' \
    "$(caos hash /cas/args/base)" > "$r/outer/.caos-expr"
  caos put "$r" /cas/fixture || fail "publishing the fixture"
  caos get /cas/fixture/outer || fail "reading the fixture back"
  [ ! -e /cas/fixture/outer/tool ] \
    || fail "fixture stale: outer/tool exists in the tree, so this proves nothing"
  caos eval-path-then /cas/fixture --eval=outer/tool --then:hash="$(next lifted)"
  ;;

lifted)
  # --result is the image `outer/tool` resolved to. Running it is the proof
  # that the lifted entry is a usable base, not merely a tree that resolved.
  echo "  outer/'s expression ran; outer/tool resolved to $(caos hash /cas/args/result)" >&2
  caos run-request-then \
    "$(caos prepare-request --base:hash="$(caos hash /cas/args/result)" --color=blue)" \
    --then:hash="$(next nested)"
  ;;

nested)
  out=$(result_text)
  has_line "$out" '  color = blue' \
    || fail "the lifted entry did not run as an image: $out"
  echo "  ok: outer/'s expression ran, then tool/'s — neither is in the tree as a base" >&2

  printf 'hello: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
