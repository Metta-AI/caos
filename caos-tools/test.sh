#!/usr/bin/env bash
#@doc Build the test stack image from the tree and run the whole test suite —
#@doc the unit tests and every tests/<name> integration suite — as cached
#@doc jobs: an unchanged test never re-runs. Returns the report: a line per
#@doc test with its time and the hash of its record, the last few lines of
#@doc every failing test, and a pass/fail banner. Pass a record's hash to
#@doc `test-result` for that test's full output or its inner-stack logs.
#@doc Nothing is handed in from the host: the stack under test is compiled
#@doc from these sources, inside workers
#@arg [test-salt] Re-run every test while leaving the build a cache hit — any fresh value (e.g. $(date --iso=s)) re-keys the tests and nothing else.
#
# THE test suite, as a caos worker (design/test-stack-image.md). Its interface
# is a TOOL's interface: the workspace tree as --in, and optionally an API key
# and a test filter — every script it runs comes from the workspace itself, so
# the suite tests exactly the harness the tree carries. Keyed on all of it: a
# full-suite cache hit means literally nothing changed; salt to force. The
# result is {report, results/<test>/...}.
#
# THREE STAGES, one script, selected by a curried --stage (the
# caos-tools/build.sh pattern).
#
#   suite      (default) run-then THE BUILD TOOL (caos-tools/build.sh — the
#              same job an agent's `build` call fires, sharing its cache),
#              whose result is the TEST STACK IMAGE
#   stage3     the image's ref is only knowable once the build has run, so
#              fanning out over it needs its own stage: one job per
#              tests/<name>/cli.sh, each running that image with the per-test
#              runner (tests/lib/run-test.sh) as worker1
#   summarize  the `then` of the fan-out — assemble the report
#
# Test = build + run tests, literally. There is no --bins: the tree under test
# is compiled from source inside the build job, so nothing crosses from the
# host but the tree.
set -euo pipefail

fail() { echo "TEST FAIL: $*" >&2; exit 1; }

# How much of a failing test's output the report inlines. Enough to carry the
# stage heading, the assertion and its diagnostic — not enough to bury the
# other tests when several fail at once. The whole thing is one `test-result`
# call away, which is the point: the report is an INDEX, not an archive.
EXCERPT_LINES=20

# Args are lazy placeholders — fetch before reading. The initial invocation
# carries no --stage, so the fetch fails and we default to the first stage.
stage=suite
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi

case "$stage" in

suite)
  caos get /cas/args/in
  caos get /cas/args/in/caos-tools

  # The pruned tree — just what cargo reads — feeds the wrapper tests
  # (cargo-self, unit), whose jobs must not re-key on non-Rust edits.
  mkdir /tmp/build-ws
  for e in Cargo.toml Cargo.lock rust-toolchain.toml crates; do
    if [ -e "/cas/args/in/$e" ]; then ln -s "/cas/args/in/$e" "/tmp/build-ws/$e"; fi
  done
  caos put /tmp/build-ws /cas/build-ws

  build=$(caos curry /cas/std/bash -- \
    "--worker1:@=/cas/args/in/caos-tools/build.sh") || fail "currying the build tool"

  # `stage3` reads the workspace as /cas/args/in: run-then hands its `then` the
  # same --in it ran over, so the tree needs no second binding.
  fwd=("--worker1:@=/cas/args/worker1" --stage=stage3 "--build-ws:@=/cas/build-ws")
  if [ -e /cas/args/api-key ]; then fwd+=("--api-key:@=/cas/args/api-key"); fi
  if [ -e /cas/args/only ]; then fwd+=("--only:@=/cas/args/only"); fi
  # --test-salt: re-run the TESTS without re-running anything else. CAOS_SALT
  # cannot do this — it threads into every sub-run, so it re-keys the reduce, the
  # compile, the std publish and the image alongside the tests (measured: 47s
  # against 35s), and fills the cache with entries nothing will hit again. This
  # one rides only in each per-test wrapper, so the build stays a cache hit.
  #
  # It exists because the alternative people reach for is editing a tracked file
  # to bust the key — which is how `# rekey <timestamp>` once ended up committed
  # to tests/lib/run-test.sh.
  if [ -e /cas/args/test-salt ]; then fwd+=("--test-salt:@=/cas/args/test-salt"); fi

  stage3=$(caos curry /cas/std/bash -- "${fwd[@]}") || fail "currying the fan-out stage"
  caos run-then /cas/args/in -- --run="$build" --then="$stage3"
  ;;

stage3)
  # The `then` of the build tool: --result is THE TEST STACK IMAGE
  # (design/test-stack-image.md). Curry the per-test runner onto it, select the
  # tests, and map-then over them — one stack per test, each carrying the
  # binaries and a SEEDED std, so nothing has to be handed in.
  #
  # There used to be a stage between this and the build: a single job that
  # published std into the host registry before the fan-out, because nineteen
  # stacks starting on a cold registry all missed the same memo and all baked
  # the toolchain, filling the outer pool with 20-minute jobs until whatever was
  # still queued died on the pending timeout (`no runner for arg_tree (waited 900s)`,
  # measured). That whole stage is gone: std is published ONCE, when the image
  # is built (design/one-stack-image.md, "The seed"), so there is no cold-start
  # herd left to serialize.
  #
  # Per-test jobs key on (the image digest, the test's own tree, the runner
  # script). A source edit moves the image and re-keys every test, which is
  # what a binaries change already did. The std-manifest closure rules are
  # gone with the wrapper: there is no longer a per-test choice of which
  # binaries and images ride along, because they all ride in the one image.
  caos get /cas/args/result
  # One level further: the per-test subsets below symlink to individual entries,
  # so `std` and `bin` must exist as placeholders for `caos put` to resolve them
  # by recorded hash. Placeholders only — no content is fetched here.
  caos get /cas/args/result/std
  caos get /cas/args/result/bin
  # The seed records (design/caos-expr.md, Phase 3), when the build carried them.
  # A whole-tree placeholder — every wrapper gets the same one, so the inner
  # core-seeder-runner answers flake-builder's `run docker://seeded`.
  if [ -e /cas/args/result/seed ]; then caos get /cas/args/result/seed; fi
  caos get /cas/args/in
  caos get /cas/args/in/tests
  caos get /cas/args/in/tests/lib

  # The per-test map worker is curried HERE, where the image is a genuine
  # --result tree. Passing the image itself as a curried arg to a later stage
  # does not work: `caos curry /cas/args/<argname>` curries over the arg NODE,
  # so the resulting worker inherits this job's own bindings (observed: a map
  # job whose args were `image in worker1`, running as uid 1000 because the
  # image's root grant was not in play either).
  #
  # run-test.sh is the one stage of the suite that is NOT this file, and cannot
  # be: it runs INSIDE the test stack, under an env where `caos` is the TESTED
  # client aimed at the inner server, so it can neither fetch this file's args
  # nor reach the outer stack (test-stack/worker materializes its args before
  # flipping the env).
  map=$(caos curry /cas/args/result/image -- \
    "--worker1:@=/cas/args/in/tests/lib/run-test.sh") || fail "currying the per-test runner"

  # The test selection: every tests/<name> with a cli.sh — or just the names in
  # --only (a filtered suite; its per-test jobs share their cache with full
  # runs). Each child is a wrapper {test, workspace?, api-key?} carrying only
  # what that test needs beyond the image. Symlinks into the args materialize
  # nothing — `caos put` resolves them to recorded hashes.
  only=""
  if [ -e /cas/args/only ]; then
    caos get /cas/args/only
    only=" $(cat /cas/args/only) "
  fi

  # --test-salt rides in EVERY per-test wrapper and nowhere else, so a fresh
  # value re-runs all the tests and leaves the build a cache hit. Nothing reads
  # the file: its presence in the wrapper is what moves the per-test key. Do not
  # "clean up" the unused write — it is the whole mechanism.
  salt=""
  if [ -e /cas/args/test-salt ]; then
    caos get /cas/args/test-salt
    salt=$(cat /cas/args/test-salt)
  fi

  mkdir /tmp/sel
  for d in /cas/args/in/tests/*/; do
    t=$(basename "$d")
    if [ -n "$only" ]; then
      case "$only" in *" $t "*) ;; *) continue ;; esac
    fi
    caos get "/cas/args/in/tests/$t"
    [ -e "/cas/args/in/tests/$t/cli.sh" ] || continue
    mkdir -p "/tmp/sel/$t"
    ln -s "/cas/args/in/tests/$t" "/tmp/sel/$t/test"
    if [ -n "$salt" ]; then printf '%s' "$salt" > "/tmp/sel/$t/salt"; fi
    # The seed records ride into EVERY wrapper (unlike the per-test std subset):
    # they carry the flake-builder image the inner seeder answers with, and any
    # test that builds a flake reaches flake-builder transitively. A symlink put,
    # so nothing moves; test-stack/worker seeds refs/caos/seed from it.
    if [ -e /cas/args/result/seed ]; then
      ln -s /cas/args/result/seed "/tmp/sel/$t/seed"
    fi

    # WHAT THIS TEST REACHES FOR, and nothing else. `uses-std` names the
    # /cas/std entries its jobs resolve; `uses-bin` the binaries it copies out
    # of CAOS_BIN_DIR to build its own curries. Each becomes a subtree of
    # symlinks into the build result, so the wrapper carries those entries BY
    # HASH — `caos put` resolves a symlink into /cas to its recorded hash, so
    # not one byte moves here.
    #
    # This is the whole mechanism. std used to be baked into the image, so any
    # worker binary moved the image and re-keyed all twenty tests; now a test's
    # key holds what it named. A std/rgrep source edit moves /cas/std/rgrep,
    # which the tests that name it re-key — the rest are hits.
    #
    # Undeclared is UNAVAILABLE, deliberately: an unnamed std entry will not
    # resolve and an unnamed binary will not copy. Both fail loudly inside the
    # test, where a wrong declaration can only cost a red run, never a stale
    # green one. It cost four rounds of red to get these lists right, and every
    # one was the same rule:
    #
    #   THE GIT CLOSURE COVERS TREE REFERENCES AND NOTHING ELSE.
    #
    # Fetching a std entry brings its subtrees, so a curry's `args` ride along.
    # Three things do NOT, and each has to be named explicitly:
    #
    #   - A CURRY'S BASE. `std/deep-deps/base` is a BLOB holding the base
    #     image's hash, not a reference to it, so declaring `deep-deps` without
    #     `runner` yields "object not found" on the runner tree.
    #   - A HASH BOUND AS A LITERAL. `std/rustc` binds `--cargo=<hash>` the same
    #     way, so rustc needs `cargo`. And a `.caos-expr` SOURCE entry names its
    #     builders the same way: `std/rgrep` builds via `run /std/rustc` over
    #     `/std/runner`, so a test resolving `rgrep` needs `rustc cargo runner`.
    #   - A NAME THE SERVER LOOKS UP. `std/bash` is a flake tree, and running one
    #     makes the server resolve `flake-builder` BY NAME — "std library has no
    #     flake-builder".
    #
    # And grep the CLIENT too, not just the test: `caos-cli talk` resolves
    # runner/bash-tool/llm-call/llm-step/rgrep/bash from constants in chat.rs,
    # and a Rust worker builds its path with `std_image("bash")`
    # (tests/commit) — neither shows up in a search for /cas/std in the test
    # directory.
    mkdir -p "/tmp/sel/$t/std" "/tmp/sel/$t/bin"
    # The std ROOT expression rides into EVERY subset. std is published
    # un-deepened now (design/caos-expr.md), and `std/.caos-expr` —
    # `run deep-deps -- --in:@=.` — is what deepens it on resolution, so a subset
    # without it resolves nothing at all. Two consequences the `uses-std` lists
    # above have to carry, and both are why every list names `deep-deps runner`:
    #   - the transform deepens the WHOLE subset, so each list must be closed
    #     under DEPS (an entry whose `../x` target is absent fails the deepen);
    #   - the seeded deep-deps result is a curry over the runner delta, whose
    #     base is a HASH BLOB — the runner tree does not ride along with it.
    ln -s /cas/args/result/std/.caos-expr "/tmp/sel/$t/std/.caos-expr"
    if [ -e "$d/uses-std" ]; then
      caos get "/cas/args/in/tests/$t/uses-std"
      for e in $(cat "$d/uses-std"); do
        ln -s "/cas/args/result/std/$e" "/tmp/sel/$t/std/$e"
      done
    fi
    if [ -e "$d/uses-bin" ]; then
      caos get "/cas/args/in/tests/$t/uses-bin"
      for e in $(cat "$d/uses-bin"); do
        ln -s "/cas/args/result/bin/$e" "/tmp/sel/$t/bin/$e"
      done
    fi

    case "$t" in
      cargo-self | unit-*)
        # Dogfood the tree under test — the PRUNED build tree (what cargo
        # reads, the compile's own input), so only Rust-relevant edits re-key
        # these, exactly like the compile itself. A glob, so the four unit-*
        # tests (test, clippy, doc, fmt) all get the same tree and re-key
        # together — a new one needs no edit here.
        ln -s /cas/args/build-ws "/tmp/sel/$t/workspace"
        ;;
      std-lint)
        # The literal-tree lints check the checked-in std copies against their
        # sources of truth ACROSS the tree, so this test gets the whole
        # workspace and honestly re-keys on any edit to it. It is a fast lint,
        # so that trade is fine.
        ln -s /cas/args/in "/tmp/sel/$t/workspace"
        ;;
      chat-online)
        # The real-API key, when the suite was given one: same key, same cache
        # key — only this test re-keys when it rotates. Without one the test's
        # cli.sh self-skips.
        if [ -e /cas/args/api-key ]; then
          caos get /cas/args/api-key
          cp /cas/args/api-key /tmp/sel/chat-online/api-key
        fi
        ;;
    esac
  done
  caos put /tmp/sel /cas/sel

  # The build's own elapsed seconds ride into the summariser so the report can
  # show them. Curried HERE because the summariser is the `then` of the map — it
  # receives --children and nothing else.
  #
  # --start-time is the clock for the test phase, and it is taken HERE, one line
  # before the fan-out fires, because this is the last point that certainly runs
  # when the tests might. The summariser subtracts it from its own `now`.
  #
  # The phase cannot be recovered from the tests themselves: a test's start and
  # end are files in its RESULT, so a cache hit replays the pair from whenever it
  # last ran, and min/max across twenty records then spans back to that run (2306s
  # against a 38s invocation, seen). Measured across two jobs that really ran, the
  # number is right in both directions — a fan-out of cache hits is genuinely
  # quick, and says so.
  #
  # A timestamp in args means the summariser never caches. That is the point: it
  # is one cheap container, and it only runs at all when this stage does.
  then_img=$(caos curry /cas/std/bash -- \
    "--worker1:@=/cas/args/worker1" --stage=summarize \
    "--build-time:@=/cas/args/result/time" "--start-time=$(date +%s)") \
    || fail "currying the summarize stage"
  caos map-then /cas/sel -- --map="$map" --then="$then_img"
  ;;

summarize)
  # Every test job's result tree arrives under --children (by test name):
  # {verdict, output, server.log, runnerd.log, ...} — the complete record.
  # Assemble the report — one PASS/FAIL line per test, ending in an OK/FAILED
  # banner — and carry the children through verbatim as `results` (a symlink
  # put: recorded-hash reuse, no bytes move). The suite job itself always
  # SUCCEEDS with a report; the caller decides what a FAILED banner means.
  # Failures are values here so one broken test never hides the others.
  caos get /cas/args/children
  caos get /cas/args/build-time
  caos get /cas/args/start-time
  mkdir -p /tmp/rep
  passn=0 failn=0 abortn=0
  {
    # Collected first, printed after: the column width is only known once every
    # child has been read.
    names=() marks=() times=() hashes=() excerpts=()
    width=0
    for c in /cas/args/children/*; do
      t=$(basename "$c")
      caos get "/cas/args/children/$t"
      caos get "/cas/args/children/$t/verdict"
      # The test's own wall time, so the report says which test is the long
      # pole. A suite is as slow as its slowest test once the pool is saturated,
      # and that fact was previously only reachable by correlating inner redis
      # logs by hand.
      caos get "/cas/args/children/$t/seconds"
      s=$(cat "$c/seconds")
      excerpt=""
      if grep -q "^RUN-TEST: PASS" "$c/verdict"; then
        mark="✓"; passn=$((passn + 1))
      else
        # A test that ABORTED never reached an assertion — an unguarded command
        # tripped `set -e` (tests/lib/run-test.sh). That is a different report
        # to "the assertion ran and disagreed", and it points somewhere else:
        # at the cli.sh itself, or at the code it was driving. `!` not `✗`.
        if grep -q "(aborted)" "$c/verdict"; then
          mark="!"; abortn=$((abortn + 1))
        else
          mark="✗"
        fi
        failn=$((failn + 1))
        # THE LAST LINES, NOT THE FIRST. Every tests/<name>/cli.sh narrates its
        # way down — `echo "== step ==" >&2` per stage — and ends at `fail`, so
        # the head of a failing output is the fixture setup that worked and the
        # tail is the assertion that didn't, with the stage heading right above
        # it. Checked against a deliberately failed test: the head was "== build
        # the fixture worker ==", the tail was the FAIL line.
        caos get "/cas/args/children/$t/output"
        excerpt=$(tail -n "$EXCERPT_LINES" "$c/output")
      fi
      names+=("$t"); marks+=("$mark"); times+=("$s"); excerpts+=("$excerpt")
      # The record's own hash, so `test-result <hash>` can read the WHOLE thing
      # — full output, inner-stack logs — without the reader first having to
      # learn how to address a subtree of the suite result. It is printed for
      # passes too: a green test whose logs you want is the same lookup.
      hashes+=("$(caos hash "/cas/args/children/$t")")
      # `if`, not `[ … ] && …`: this is the last command in the loop body, where
      # a false test would end the loop AND the script under `set -e`.
      if [ ${#t} -gt "$width" ]; then width=${#t}; fi
    done

    # The build and the tests as two comparable lines, the tests indented beneath
    # theirs. The test phase is now minus the stamp stage3 took as it fired the
    # fan-out: measured across two jobs that ran, never recovered from cached
    # values.
    echo "build ($(cat /cas/args/build-time)s)"
    echo "tests ($(($(date +%s) - $(cat /cas/args/start-time)))s)"
    # A mark rather than a word, and no colour: this report is a VALUE in a git
    # tree, so a worker cannot know whether whoever eventually reads it is a
    # terminal, and ANSI escapes would be baked into the artifact and into every
    # log that ever prints it.
    for i in "${!names[@]}"; do
      printf '  %s %-*s %4s  %s\n' \
        "${marks[$i]}" "$width" "${names[$i]}" "${times[$i]}s" "${hashes[$i]}"
    done
    echo
    if [ "$failn" -eq 0 ]; then
      echo "SUITE OK: $passn/$((passn + failn)) passed"
    elif [ "$abortn" -eq 0 ]; then
      echo "SUITE FAILED: $passn/$((passn + failn)) passed"
    else
      # Aborts called out on the banner line: they are the ones whose excerpt
      # is a stack trace of the harness rather than an assertion, and knowing
      # that before reading the excerpts saves a wrong first guess.
      echo "SUITE FAILED: $passn/$((passn + failn)) passed ($abortn aborted)"
    fi
    # `!` is a new mark; say what it means, but only when one is on the board.
    if [ "$abortn" -gt 0 ]; then
      echo "(! = aborted before asserting: an unguarded command failed. Read the"
      echo " cli.sh, or what it was driving — not an assertion that never ran.)"
    fi
    # A duration is a property of a RUN; a result is a property of its INPUTS.
    # Caching the one inside the other means an unchanged test replays whatever
    # it happened to cost when it last actually ran — which is the number you
    # will keep seeing until something re-keys it, however much faster the tests
    # have since become. Said out loud, because it read as "the tests are still
    # slow" the first time a report replayed times measured while the engine was
    # unpacking a new image across twenty concurrent stacks.
    echo "(times are each test's LAST ACTUAL RUN; an unchanged test is a cache"
    echo " hit and replays the time it recorded then."
    echo " Pass \`--test-salt=\$(date --iso=s)\` to rerun all tests."
    echo " The hash is the test's record: \`test-result <hash>\` prints its full"
    echo " output, \`test-result <hash> --log=server\` an inner-stack log.)"

    # The excerpts go LAST, after the banner and the notes, because both
    # readers of this report truncate by keeping the TAIL (run-tool's caller
    # and the agent harness's tree_tool_result_block both cut the head at
    # 100 KB). A twenty-test suite where half of them fail must not spend its
    # budget on the passing table and lose every diagnostic.
    for i in "${!names[@]}"; do
      if [ -z "${excerpts[$i]}" ]; then continue; fi
      echo
      echo "---- ${names[$i]}: last $EXCERPT_LINES lines (test-result ${hashes[$i]}) ----"
      # Indented, so a test's output can never be mistaken for the report's own
      # structure — an inner "SUITE OK" would otherwise read as this suite's.
      #
      # A bash loop, not `sed`: std/bash's contents are bash, coreutils,
      # diffutils, gnugrep, findutils and jq — there is NO gnused. This ran
      # green for two suites before anything noticed, because the loop only
      # executes when a test FAILS, which is precisely when the report must
      # not be the thing that breaks.
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
