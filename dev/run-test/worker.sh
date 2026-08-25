#!/bin/bash
# One test, bracketed by three jobs. Mapped over the per-test trees the fan-out
# assembles (dev/run-tests/worker.sh), one child per tests/<name>.
#
# THREE STAGES, one script, selected by a curried --stage — the same shape
# dev/run-tests uses, and for the same reason: what each stage needs is only
# knowable once the previous one's work has finished.
#
#   eval     (default) `--in` is the WRAPPER the fan-out built: the test's own
#            deepened tree at `test`, plus `workspace` for the one test that
#            needs it. Evaluating `test` yields the test's ARG TREE.
#            A worker may not evaluate — its `caos` has `eval-path-then` and no
#            `eval-path` — so this records a continuation and exits. Every test
#            evaluates in parallel, one map child each.
#   launch   the `then` of that: --result is the ArgTree. Bind what this RUN
#            supplies — the client under test, the salt, and a workspace when
#            the wrapper carries one — and run it.
#   verdict  the `then` of THAT: --result is what the test returned, or
#            --error what it failed with.
#
# WHY EVALUATION IS PURE, at the cost of this extra hop. A test's expression
# could be `run`-valued, and then evaluating it would BE running it and `launch`
# would not exist. Two things are worth one container per test:
#
#   * THE REQUEST BECOMES NAMEABLE. `launch` prints the ArgTree it is about to
#     run, so "which request is this test" is a fact in the log rather than
#     something only re-evaluation can tell you — which is what re-running one
#     test, and polling /status for a running one, both need.
#   * EVALUATION FAILURE SEPARATES FROM TEST FAILURE. With a `run`-valued
#     expression both arrive as the same `eval-path "." in continuation …:
#     worker failed` and nothing distinguishes a broken `.caos-expr` from a
#     failed assertion. Here a broken expression is caught at `launch` and says
#     so; a failing test is caught at `verdict`.
#
# `--catch` ON BOTH IS LOAD-BEARING. Without it a failure fails this job, which
# fails the map, which discards every OTHER test's result — one broken test
# would report as a suite with no results at all. Caught, a failure is a value:
# this test's FAIL, with the failing worker's own stderr as the diagnostic.
set -euo pipefail

fail() { echo "RUN-TEST FAIL: $*" >&2; exit 1; }

# Write the report's {verdict, seconds, output} and finish. `--start` is stamped
# at `eval` and carried, so `seconds` is WALL time across the test's whole
# chain — which is what "which test is the long pole" wants to mean.
publish() { # <verdict-line> <output-file>
  caos get /cas/args/start || fail "reading --start"
  mkdir -p /tmp/out
  printf '%s\n' "$1" > /tmp/out/verdict
  cp "$2" /tmp/out/output
  echo $(($(date +%s) - $(cat /cas/args/start))) > /tmp/out/seconds
  caos put /tmp/out /cas/out
}

stage=eval
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi

case "$stage" in

eval)
  # `cli` and `test-salt` are FORWARDED, not inherited. `/cas/args/base` is the
  # reserved base entry — the bash image — not this job's ArgTree, so currying
  # onto it carries none of what the fan-out bound alongside it. Anything a
  # later stage reads has to be re-bound by name.
  next=$(caos curry --base:@=/cas/args/base --worker1:@=/cas/args/worker1 \
    --stage=launch --cli:@=/cas/args/cli --test-salt:@=/cas/args/test-salt \
    "--start=$(date +%s)") || fail "currying the launch stage"
  caos eval-path-then /cas/args/in --eval=test --then:hash="$next" --catch
  ;;

launch)
  # Args are lazy placeholders: `--start` has to be FETCHED before it can be
  # read, and an unfetched one reads as empty — which surfaces two containers
  # later as `1787683099 - : arithmetic syntax error` in the elapsed sum.
  caos get /cas/args/start || fail "reading --start"
  # Expand the wrapper before asking what it holds: an unfetched directory
  # answers "no" to every question about its contents, so the
  # `workspace` check below would silently bind nothing.
  caos get /cas/args/in || fail "expanding the wrapper"
  if [ -e /cas/args/error ]; then
    # The EXPRESSION is broken — a missing `DEEP-DEPS/<name>`, a path that does
    # not resolve — so no test ever ran. Said plainly, because the failure a
    # reader is looking for is in the `.caos-expr`, not in any worker.
    caos get /cas/args/error || fail "reading --error"
    { echo "FAIL: this test's .caos-expr could not be evaluated — no test ran."
      echo "--- the failure"
      cat /cas/args/error
    } > /tmp/eval-failed
    cat /tmp/eval-failed >&2
    publish "RUN-TEST: FAIL" /tmp/eval-failed
    exit 0
  fi
  # --result IS the test's ArgTree — currying returns an ArgTree, and an ArgTree
  # is what runs (SPEC, "Forming an ArgTree": "an image is just one arg"). So
  # `run-then --run:` names it directly: the server's `run_image` unwraps the
  # curry and merges its bound args, and adds `salt` and `secret-hash` from THIS
  # job, which is how a test inherits the run's context.
  #
  # NOT `run-request-then`. That verb runs an ArgTree by IDENTITY — for a caller
  # that already knows the exact request hash and must run that one unmodified,
  # which is what `prepare-request` is paired with. We have an ArgTree to run,
  # not a request identity to honour, and using it here silently dropped every
  # bound arg: nothing unwrapped the curry, so `args` and `.caos-curry` arrived
  # as two arguments of those names and tests/lib died on `/cas/args/test`.
  #
  # WHAT THIS RUN SUPPLIES IS BOUND HERE, not staged into the test's tree. A
  # test's `.caos-expr` names only what sits beside it in its own directory;
  # the client under test and the salt are properties of the RUN, and they
  # reach the test by being passed when its ArgTree is called — which is what
  # currying is (SPEC, "Currying": takes an ArgTree and args, returns an
  # ArgTree). `curry` fails on a rebind, so a test that bound one of these
  # itself gets a loud error rather than a silent shadow.
  #
  # `cli` and `test-salt` are the same for every test and ride on this image,
  # curried by the fan-out. `workspace` differs per test, so it rides in the
  # wrapper — and is passed only to the tests that were given one.
  bind=(--cli:@=/cas/args/cli --test-salt:@=/cas/args/test-salt)
  if [ -e /cas/args/in/workspace ]; then bind+=(--workspace:@=/cas/args/in/workspace); fi
  req=$(caos curry --base:hash="$(caos hash /cas/args/result)" "${bind[@]}") \
    || fail "binding this run's arguments onto the test"

  # Printed before the run: this is the only place that knows it, and it is what
  # anything wanting to watch or re-run this one test must name.
  echo "run-test: arg tree $req" >&2
  next=$(caos curry --base:@=/cas/args/base --worker1:@=/cas/args/worker1 \
    --stage=verdict "--start=$(cat /cas/args/start)") || fail "currying the verdict stage"
  caos run-then /cas/args/in/test --run:hash="$req" --then:hash="$next" --catch
  ;;

verdict)
  if [ -e /cas/args/error ]; then
    caos get /cas/args/error || fail "reading --error"
    # The error text is the failing worker's stderr, relayed by the runner — so
    # the report's excerpt is the diagnostic from wherever in the test's chain
    # it actually broke, not this job's view of it.
    publish "RUN-TEST: FAIL" /cas/args/error
  else
    caos get /cas/args/result || fail "reading --result"
    # A test's result is conventionally a BLOB of its own narration, which is
    # what `test-result` prints. A test that returns a tree instead has nothing
    # to inline, and says so rather than dumping a directory listing.
    if [ -f /cas/args/result ]; then
      publish "RUN-TEST: PASS" /cas/args/result
    else
      echo "(this test returned a tree, not a narration blob)" > /tmp/no-narration
      publish "RUN-TEST: PASS" /tmp/no-narration
    fi
  fi
  ;;

*)
  fail "unknown --stage: $stage"
  ;;
esac
