#!/usr/bin/env bash
# The suite, as a job on the DEV STACK: deepen the workspace so every test's
# DEPS become mounts, fan out one job per tests/<name>, assemble the report.
#
# WHERE THIS RUNS is the whole point. `caos-tools/test` stands a dev stack up
# inside its own worker and then asks THAT stack to run this — so the per-test
# jobs are cached and dispatched by the stack built from the tree under test,
# not by the host's. A test that has not changed is a hit there; the host only
# ever sees one job, this one.
#
# NO INNER STACK PER TEST. A test is a plain worker on the dev stack, which IS
# the stack under test, so there is nothing left to bring up. What used to make
# that necessary — a client of one world driving a server of another — is
# handled by the dev stack being built `CAOS_WORLD=test` while the host stays
# `host`, so the guard still has two worlds to tell apart (tests/world-guard).
#
# THREE STAGES, one script, selected by a curried --stage. Each exists because
# the thing it needs is only knowable once the previous run finished — a worker
# delegates its continuation rather than blocking on a run (design/map-then.md).
#
#   suite      (default) resolve the deep-deps image through its sentinel and
#              run it over the WORKSPACE
#   fanout     the `then` of that: --result is the deepened tree, so each test's
#              deps are at tests/<name>/DEEP-DEPS/<mount> and the wrappers can
#              be built — one job per tests/<name>
#   summarize  the `then` of the fan-out — assemble the report
set -euo pipefail

fail() { echo "RUN-TESTS FAIL: $*" >&2; exit 1; }

# How much of a failing test's output the report inlines: enough for the stage
# heading, the assertion and its diagnostic, not enough to bury the other tests.
EXCERPT_LINES=20

# Args are lazy placeholders — fetch before reading. The first invocation
# carries no --stage, so the fetch fails and we default to the first.
stage=suite
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi

# The optional args every stage has to hand to the next, because a `then`
# receives only `--in`/`--result` — anything else must be curried through by
# name. One function so a new one is added in one place rather than three, which
# is how the old version of this lost `--only` for a while.
forwarded() {
  for a in api-key only test-salt; do
    if [ -e "/cas/args/$a" ]; then printf '%s ' "--$a:@=/cas/args/$a"; fi
  done
}

case "$stage" in

suite)
  # THE IMAGE A TEST RUNS IN — the same one this script is running in, resolved
  # again as a CLEAN image. Currying the map off /cas/args/base instead would
  # hand each test this job's own bindings (observed once as a map job whose
  # args were `image in worker1`, running as the wrong uid).
  #
  # `dev/test-stack` is a source directory carrying a `.caos-expr`, not an
  # image, and turning one into the other is evaluation — which a worker may not
  # do itself, because `eval_path` on a `run` BLOCKS on the result and a worker
  # must never hold a slot waiting (design/map-then.md). `eval-path-then` is the
  # supported way out: the server walks the expressions on a request thread and
  # hands the result to the `then`.
  #
  # NOT a hand-rolled sentinel request. The older shape here formed a seeded
  # core item's arg tree by hand — strip the `.caos-expr`, name
  # `docker://seeded-…` — and CLAUDE.md records what that costs: it has to match
  # build-builtins.sh's seed record EXACTLY, `strip_caos_expr` included, nothing
  # checks that agreement, and a disagreement is a job that waits in silence
  # until the server 503s. Evaluation asks for the thing by name instead.
  next=$(caos curry --base:@=/cas/args/base \
    "--worker1:@=/cas/args/worker1" --stage=deepen \
    "--ws:@=/cas/args/in" "--cli:@=/cas/args/cli" \
    $(forwarded)) || fail "currying the deepen stage"
  caos eval-path-then /cas/args/in --eval=dev/run-test --then:hash="$next"
  ;;

deepen)
  # `--result` is the test-stack IMAGE. Now evaluate the tree's own root
  # expression — `run --base:@=std/deep-deps --in:@=.` — which is what turns
  # every directory's `DEPS` into `DEEP-DEPS/<name>` mounts. Same mechanism,
  # second use: the root expression is just another path to evaluate.
  next=$(caos curry --base:@=/cas/args/base \
    "--worker1:@=/cas/args/worker1" --stage=fanout \
    "--ws:@=/cas/args/ws" "--cli:@=/cas/args/cli" \
    "--runner:@=/cas/args/result" \
    $(forwarded)) || fail "currying the fan-out stage"
  caos eval-path-then /cas/args/ws --eval=. --then:hash="$next"
  ;;

fanout)
  # `--result` is the DEEPENED workspace, so a test's own `DEPS` have already
  # become `tests/<name>/DEEP-DEPS/<mount>` — which is what a test's
  # `.caos-expr` names its dependencies by. `--ws` is the workspace as it
  # arrived, used only to enumerate which tests exist.
  caos get /cas/args/ws
  caos get /cas/args/ws/tests
  caos get /cas/args/result
  caos get /cas/args/result/tests
  caos get /cas/args/cli
  caos get /cas/args/runner

  # The image mapped over the per-test trees: dev/run-test, which evaluates one
  # and turns the outcome into a verdict. It is the same image for every test —
  # what differs per test is the TREE it is handed, because a test is an entry
  # and running it is evaluating it.
  map=$(caos hash /cas/args/runner) || fail "reading the per-test image"

  only=""
  if [ -e /cas/args/only ]; then
    caos get /cas/args/only
    only=" $(cat /cas/args/only) "
  fi
  # The salt is written for EVERY test, empty when none was passed, because a
  # test's `.caos-expr` binds `--test-salt:@=test-salt` unconditionally and an
  # absent path
  # is an evaluation error rather than an omitted argument. NOT `salt`: that name
  # is RESERVED, and the dispatcher merges the run's own salt last
  # (compute.rs, `run_image`), so a per-test value bound under it is silently
  # overwritten and never reaches the key — two different --test-salt values
  # would produce one request and the test would not re-run. Binding it is what
  # makes a fresh value re-run the tests: the file changes the tree, which
  # changes what the tree evaluates to, which changes the test's own ArgTree.
  # Nothing READS it.
  salt=""
  if [ -e /cas/args/test-salt ]; then
    caos get /cas/args/test-salt
    salt=$(cat /cas/args/test-salt)
  fi

  mkdir /tmp/sel
  for d in /cas/args/ws/tests/*/; do
    t=$(basename "$d")
    if [ -n "$only" ]; then
      case "$only" in *" $t "*) ;; *) continue ;; esac
    fi
    # Expand this test's directory before asking what is in it: args arrive as
    # lazy placeholders, so an unfetched directory answers "no" to every
    # question about its contents.
    caos get "$d" || fail "expanding tests/$t"
    # A TEST IS AN ENTRY WITH A SCRIPT. `tests/lib` is an entry too — the image
    # client tests DEP on — but it has no `worker.sh`, so nothing maps over it.
    if [ ! -e "$d/.caos-expr" ] || [ ! -e "$d/worker.sh" ]; then continue; fi

    # THE CHILD IS THE TEST'S OWN DEEPENED TREE, entry by entry, plus what
    # varies per RUN rather than per tree. Symlinked rather than copied: `caos
    # put` resolves a symlink to the recorded hash, so this moves no bytes — but
    # it has to be entry by entry, because linking the directory itself would
    # nest the test one level down and its `.caos-expr` would no longer be at
    # the root where `--eval=.` looks for it.
    src=/cas/args/result/tests/$t
    caos get "$src"
    mkdir -p "/tmp/sel/$t"
    for e in "$src"/* "$src"/.[!.]*; do
      [ -e "$e" ] || continue
      ln -s "$e" "/tmp/sel/$t/$(basename "$e")"
    done

    # THE TESTED CLIENT, as content rather than as something ambient: a test
    # re-keys when the client it drove changes, which is the property a suite
    # exists to have. Bound by every test's expression whether or not it runs
    # the thing.
    ln -s /cas/args/cli "/tmp/sel/$t/cli"
    printf '%s' "$salt" > "/tmp/sel/$t/test-salt"

    case "$t" in
      cargo-self | unit-* | std-lint)
        # Dogfood the tree under test: the whole workspace, for the tests whose
        # subject IS the workspace.
        ln -s /cas/args/ws "/tmp/sel/$t/workspace"
        ;;
      chat-online)
        # Always present, possibly EMPTY — same reason as the salt: the
        # expression binds it unconditionally, and this test self-skips on an
        # empty key.
        if [ -e /cas/args/api-key ]; then
          caos get /cas/args/api-key
          cp /cas/args/api-key "/tmp/sel/$t/api-key"
        else
          : > "/tmp/sel/$t/api-key"
        fi
        ;;
    esac
  done
  caos put /tmp/sel /cas/sel

  # --start-time is the clock for the test phase, taken HERE because this is the
  # last point that certainly runs when the tests might. It cannot be recovered
  # from the tests themselves: a test's start and end are files in its RESULT,
  # so a cache hit replays the pair from whenever it last ran.
  #
  # A timestamp in args means the summariser never caches. That is the point: it
  # is one cheap container, and it only runs at all when this stage does.
  then_img=$(caos curry --base:@=/cas/args/base \
    "--worker1:@=/cas/args/worker1" --stage=summarize \
    "--start-time=$(date +%s)") || fail "currying the summarize stage"
  caos map-then /cas/sel --map:hash="$map" --then:hash="$then_img"
  ;;

summarize)
  # Every test job's result arrives under --children, by test name. Assemble the
  # report and carry the children through as `results` (a symlink put, so no
  # bytes move). This job always SUCCEEDS with a report: failures are values, so
  # one broken test never hides the others.
  caos get /cas/args/children
  caos get /cas/args/start-time
  mkdir -p /tmp/rep
  passn=0 failn=0
  {
    names=() marks=() times=() hashes=() excerpts=()
    width=0
    for c in /cas/args/children/*; do
      t=$(basename "$c")
      caos get "/cas/args/children/$t"
      caos get "/cas/args/children/$t/verdict"
      caos get "/cas/args/children/$t/seconds"
      s=$(cat "$c/seconds")
      excerpt=""
      if grep -q "^RUN-TEST: PASS" "$c/verdict"; then
        mark="✓"; passn=$((passn + 1))
      else
        mark="✗"
        failn=$((failn + 1))
        # THE LAST LINES, NOT THE FIRST: a test narrates its way down and
          # ends at `fail`, so the tail is the assertion that broke with its stage
        # heading right above it.
        caos get "/cas/args/children/$t/output"
        excerpt=$(tail -n "$EXCERPT_LINES" "$c/output")
      fi
      names+=("$t"); marks+=("$mark"); times+=("$s"); excerpts+=("$excerpt")
      hashes+=("$(caos hash "/cas/args/children/$t")")
      if [ ${#t} -gt "$width" ]; then width=${#t}; fi
    done

    echo "tests ($(($(date +%s) - $(cat /cas/args/start-time)))s)"
    # A mark rather than a word, and no colour: this report is a VALUE in a git
    # tree, so a worker cannot know whether whoever reads it is a terminal, and
    # ANSI escapes would be baked into the artifact and every log that shows it.
    for i in "${!names[@]}"; do
      printf '  %s %-*s %4s  %s\n' \
        "${marks[$i]}" "$width" "${names[$i]}" "${times[$i]}s" "${hashes[$i]}"
    done
    echo
    if [ "$failn" -eq 0 ]; then
      echo "SUITE OK: $passn/$((passn + failn)) passed"
    else
      echo "SUITE FAILED: $passn/$((passn + failn)) passed"
    fi
    echo "(times are each test's LAST ACTUAL RUN; an unchanged test is a cache"
    echo " hit and replays the time it recorded then."
    echo " Pass \`--test-salt=\$(date --iso=s)\` to rerun all tests.)"

    # The excerpts go LAST, after the banner: both readers of this report
    # truncate by keeping the TAIL, so a suite where half the tests fail must
    # not spend its budget on the passing table and lose every diagnostic.
    for i in "${!names[@]}"; do
      if [ -z "${excerpts[$i]}" ]; then continue; fi
      echo
      echo "---- ${names[$i]}: last $EXCERPT_LINES lines ----"
      # A bash loop, not `sed`: std/bash has no gnused, and this path only runs
      # when a test FAILS — which is exactly when the report must not break.
      while IFS= read -r line; do printf '  %s\n' "$line"; done <<< "${excerpts[$i]}"
    done
  } > /tmp/rep/report
  ln -s /cas/args/children /tmp/rep/results
  caos put /tmp/rep /cas/out
  ;;

*)
  fail "unknown --stage: $stage"
  ;;
esac
