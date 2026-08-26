#!/usr/bin/env bash
# Runs cwd'd into a client repo with this test tree at ./test and $CAOS_CLI
# set, INSIDE a test stack (tests/lib/run-test.sh).
#
# Exercises `caos eval-path-then` (design/caos-expr.md): the SERVER-side
# evaluation continuation. A worker may not block on a run, so to evaluate a
# `.caos-expr` it records `{in, eval, then?}` and exits; the server walks the
# expression on a request thread — its own `run`s dispatched normally — and
# threads the result into `then`. The claim under test is BYTE-IDENTITY: the
# object the server builds is exactly what a client `eval-path` builds, because
# both run the SAME walk (crate `caos-eval`), one blocking at top level, the
# other blocking a server thread. Covered: a `run`-valued expression evaluated
# server-side matches the client's result hash, and `--catch` turns a broken
# eval into a value the `then` receives.
#
# WHY THIS TEST MUST GO THROUGH THE TESTED CLIENT ($CAOS_CLI) — the CLI is not
# incidental scaffolding here, it is LITERALLY BOTH OPERANDS OF THE EQUALITY
# this test asserts, and neither operand exists outside the tested client
# driving the inner stack:
#
#   * The REFERENCE side is `$CAOS_CLI eval-path pkg` — the client walking the
#     `.caos-expr` at top level (blocking on its sub-run), producing the
#     "correct" object hash. There is no other honest source for that hash:
#     it is by definition what the tested client's eval walk yields, computed
#     against THIS stack's CAS and runner. Precomputing or hand-asserting a
#     hash would test a constant, not the client's evaluator.
#   * The SUBJECT side is `$CAOS_CLI run …` dispatching a worker that calls
#     `caos eval-path-then` — driving the SERVER's copy of the same walk on a
#     request thread, dispatching its own sub-run through the inner runner.
#     Server-side evaluation is only reachable by asking a real inner stack to
#     do it, and the only way to ask is to record a job through the tested
#     client. `--catch` likewise is a server-side codepath: it can only be
#     observed by what the `then` combiner receives back from a real eval.
#
# So the property pinned down — that the server's evaluation continuation and
# the client's blocking eval are the SAME computation (crate `caos-eval`,
# differing only in where they block) and therefore agree byte-for-byte — is a
# round-trip whose two halves are both produced by the tested client against a
# live inner stack. Nothing here can be checked "directly": there is no
# artifact to lint, no script to run against $CAOS_PROJECT; the subject is the
# runtime equality of two dynamic evaluations that only the CLI can elicit, and
# comparing them out-of-band would defeat the point (both hashes must come from
# the SAME build of `caos-eval` reached two ways). The CLI is essential.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
commit() { git add -A && git -c user.email=test@caos -c user.name=caos commit -qm "$1"; }

# The bash entry this test's DEPS declared, copied into the package as its
# worker image (a `.caos-expr` names its image by a path WITHIN its subtree).
BASH=DEEP-DEPS/bash

# A package whose `.caos-expr` builds its own directory by running the bash
# worker over its files — a `run`-valued expression, so evaluating it DISPATCHES
# a sub-run (the whole point: server-side eval must be able to run things).
mkdir -p pkg
cat > pkg/build.sh <<'EOF'
#!/bin/bash
set -euo pipefail
caos get /cas/args/name
mkdir -p /tmp/out
echo "hello $(cat /cas/args/name)" > /tmp/out/greeting
caos put /tmp/out /cas/out
EOF
echo world > pkg/name
cp -r "$BASH" pkg/bash
cat > pkg/.caos-expr <<'EOF'
run --base:@=bash --worker1:@=build.sh --name:@=name
EOF

# The driver worker: record an eval-path-then over --in with the path and the
# `then` combiner curried in. Its whole body is the tail call.
cat > eval-driver.sh <<'EOF'
#!/bin/bash
set -euo pipefail
caos get /cas/args/eval-path
caos get /cas/args/then-img
args=(--eval="$(cat /cas/args/eval-path)" --then:hash="$(cat /cas/args/then-img)")
if [ -e /cas/args/catch ]; then args+=(--catch); fi
caos eval-path-then /cas/args/in "${args[@]}"
EOF

# The `then` combiner: report the eval RESULT'S HASH (and prove --in also
# arrived). On a caught failure the server binds --error instead of --result.
cat > report.sh <<'EOF'
#!/bin/bash
set -euo pipefail
caos get /cas/args/in
if [ -e /cas/args/error ]; then
  caos get /cas/args/error
  printf 'error: %s' "$(cat /cas/args/error)" > /tmp/o
else
  # cas_hash reads the recorded hash without fetching the tree.
  printf '%s' "$(caos hash /cas/args/result)" > /tmp/o
fi
caos put /tmp/o /cas/out
EOF

commit "eval-then fixtures"

echo "== reference: client eval-path of pkg ==" >&2
ref=$("$CAOS_CLI" eval-path pkg) || fail "client eval-path pkg failed"
[ "${ref%% *}" = tree ] || fail "expected a tree result, got: $ref"
refhash=${ref##* }
echo "  client eval-path pkg -> $ref" >&2

combiner=$("$CAOS_CLI" curry --base:@="$BASH" --worker1:@=report.sh) || fail "currying the combiner"

echo "== eval-path-then (server-side) yields the byte-identical object ==" >&2
out=$("$CAOS_CLI" run --base:@="$BASH" \
  --worker1:@=eval-driver.sh --in:@=. --eval-path=pkg --then-img="$combiner") \
  || fail "eval-path-then run failed"
[ "$out" = "$refhash" ] \
  || fail "server eval differs from client: got '$out', client got '$refhash'"
echo "  ok: server eval-path-then == client eval-path ($out)" >&2

echo "== --catch turns a broken eval into a value the then receives ==" >&2
# A path that does not exist makes the walk fail; --catch delivers it as --error.
out=$("$CAOS_CLI" run --base:@="$BASH" \
  --worker1:@=eval-driver.sh --in:@=. --eval-path=no-such-dir \
  --then-img="$combiner" --catch=1) \
  || fail "eval-path-then --catch should have succeeded (the error is a value)"
case "$out" in
  error:*) echo "  ok: caught eval failure delivered as --error" >&2 ;;
  *) fail "expected a caught error, got: $out" ;;
esac

echo "eval-then: ALL PASS" >&2
