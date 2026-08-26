#!/bin/bash
# tests/flake-input-loader — a WORKER test: no client, no repo, no remote.
#
# `std/flake-input-loader` (design/flake-inputs.md, "Consumer root"): a project
# that is NOT caos mounts a PINNED input into its own evaluated tree, with
# nothing of that input committed. The consumer's root `.caos-expr` says which
# input, what of it, and where it goes; the loader checks the pin against
# `flake.lock` and splices.
#
# WHAT IT MUST SHOW:
#   - the input tree lands at `--output-path`, creating parent directories the
#     consumer's tree does not have;
#   - every sibling survives the splice (it is a merge, not a replacement);
#   - the mounted tree is IDENTICAL to the tree handed in — nothing rebuilt;
#   - a pin that DRIFTS from flake.lock is refused, naming both revisions.
#
# THE REVS ARE STRINGS. The loader "cannot resolve a rev to a tree — mapping a
# rev to a tree needs a fetch, and by the time a worker runs the locator is
# already an oid" (its own header). It parses the rev out of `--expr` and
# compares it with `flake.lock`. So two distinct forty-hex strings exercise the
# check exactly, and the test needs no repo at all.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }

stage=start
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi
next() { local s=$1; shift; caos curry --base:@=/cas/args/base \
  --worker1:@=/cas/args/worker1 --stage="$s" --test-salt:@=/cas/args/test-salt \
  --loader:@=/cas/args/loader "$@"; }

SHA=1111111111111111111111111111111111111111   # what flake.lock locks
OLD=2222222222222222222222222222222222222222   # what a drifted expression pins
REPO='git+file:///synthetic/input'

# The input to splice: a plain tree, standing in for whatever a locator would
# have resolved to. The loader takes it as `--input-tree` either way.
input_tree() {
  rm -rf /tmp/input && mkdir -p /tmp/input/thing
  printf 'from the input\n' > /tmp/input/thing/file
  caos put /tmp/input /cas/input >/dev/null || fail "publishing the input tree"
  echo /cas/input
}

# The consumer: a flake (the loader refuses a non-flake — it loads a flake
# INPUT), a flake.lock locking `demo` at SHA, and a sibling that must
# survive the splice. No `.caos-expr` — evaluation strips the directive before
# the loader ever sees the tree, so a dispatch reproduces that state exactly.
consumer() {
  rm -rf /tmp/pkg && mkdir -p /tmp/pkg
  cat > /tmp/pkg/flake.lock <<LOCK
{ "nodes": { "root":   { "inputs": { "demo": "demo" } },
             "demo":   { "locked": { "type": "git", "url": "file:///synthetic/input", "rev": "$SHA" } } },
  "root": "root", "version": 7 }
LOCK
  printf '{ inputs.demo.url = "git+file:///synthetic/input"; outputs = _: { }; }\n' > /tmp/pkg/flake.nix
  printf 'keep me\n' > /tmp/pkg/sibling.txt
  caos put /tmp/pkg /cas/pkg >/dev/null || fail "publishing the consumer tree"
  echo /cas/pkg
}

# The consumer's root expression, as TEXT — the only thing the loader sees of
# it, and the only place a rev is pinned.
expr_pinning() { # <rev> -> a /cas path
  printf 'run --base:@=loader --in:@=. --expr=$CAOS_EXPR --input=demo --input-tree:@@=%s?rev=%s&dir=std --output-path=vendor/demo-std\n' \
    "$REPO" "$1" > /tmp/expr
  caos put /tmp/expr "/cas/expr-$1" >/dev/null || fail "publishing the expression"
  echo "/cas/expr-$1"
}

load() { # <rev-the-expression-pins> -> the ArgTree to run
  caos curry --base:@=/cas/args/loader \
    --expr:@="$(expr_pinning "$1")" --input=demo \
    --input-tree:@="$(input_tree)" --output-path=vendor/demo-std
}

case "$stage" in

start)
  echo "== a pinned input is mounted into the consumer's evaluated tree ==" >&2
  caos run-then "$(consumer)" --run:hash="$(load "$SHA")" \
    --then:hash="$(next spliced --input:@=/cas/input)"
  ;;

spliced)
  R=/cas/args/result; caos get -r "$R" || fail "reading the spliced tree"
  [ "$(cat "$R/vendor/demo-std/thing/file")" = "from the input" ] \
    || fail "mounted content: $(cat "$R/vendor/demo-std/thing/file" 2>&1)"
  echo "  ok: the input landed at vendor/demo-std, parent dirs created" >&2

  # A merge, not a replacement.
  [ "$(cat "$R/sibling.txt")" = "keep me" ] || fail "a sibling was lost in the splice"
  [ -e "$R/flake.lock" ] || fail "flake.lock was lost in the splice"
  echo "  ok: siblings survived" >&2

  # The mounted tree IS the tree handed in: nothing was rebuilt or copied.
  caos get /cas/args/input
  [ "$(caos hash "$R/vendor/demo-std")" = "$(caos hash /cas/args/input)" ] \
    || fail "the mount is not the input tree that was passed in"
  echo "  ok: the mount is the input tree's own oid" >&2

  echo "== a pin that drifts from flake.lock is refused ==" >&2
  # `--catch` because the refusal IS the assertion.
  caos run-then "$(consumer)" --run:hash="$(load "$OLD")" \
    --then:hash="$(next drifted)" --catch
  ;;

drifted)
  [ -e /cas/args/error ] || fail "a drifted pin was accepted"
  caos get /cas/args/error
  grep -q "is locked at $SHA in flake.lock" /cas/args/error \
    || fail "wrong error for a drifted pin: $(cat /cas/args/error)"
  grep -q "but the expression pins $OLD" /cas/args/error \
    || fail "the drift error did not name the expression's rev: $(cat /cas/args/error)"
  echo "  ok: the loader named both revisions and refused" >&2

  printf 'flake-input-loader: ALL PASS\n' > /tmp/report
  cat /tmp/report >&2
  caos put /tmp/report /cas/out
  ;;

*) fail "unknown --stage: $stage" ;;
esac
