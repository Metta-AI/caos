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
# FIVE STAGES, one script, selected by a curried --stage (the
# caos-tools/build.sh pattern). Each stage exists because the thing it needs is
# only knowable once the previous run finished — a worker delegates its
# continuation rather than blocking on a run (design/map-then.md).
#
#   suite      (default) run-then THE BUILD TOOL (caos-tools/build.sh — the
#              same job an agent's `build` call fires, sharing its cache),
#              whose result is the TEST STACK IMAGE
#   stage3     resolve the deep-deps image by running its sentinel (a worker
#              cannot eval a `.caos-expr`)
#   deepen     run it over the WORKSPACE, whose root `.caos-expr` is exactly
#              this transform — every test's DEPS resolve in the real tree
#   fanout     the `then` of that: --result is the deepened tree, so the
#              per-test wrappers can be built — one job per tests/<name>/cli.sh,
#              each running the image with tests/lib/run-test.sh as worker1
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

  build=$(caos curry /cas/args/image -- \
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

  stage3=$(caos curry /cas/args/image -- "${fwd[@]}") || fail "currying the fan-out stage"
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
  # The seed records (design/caos-expr.md, Phase 3), when the build carried them.
  # A whole-tree placeholder — every wrapper gets the same one, so the inner
  # core-seeder-runner answers flake-builder's `run docker://seeded`.
  if [ -e /cas/args/result/seed ]; then caos get /cas/args/result/seed; fi
  caos get /cas/args/in
  caos get /cas/args/in/tests
  caos get /cas/args/in/tests/lib

  # ---- deliver each test its deps WITH deep-deps (design/caos-expr.md) ------
  # `uses-std`/`uses-bin` were the hand-run prototype of this: per-test lists of
  # "what I reach for", expanded by a symlink loop, with the TRANSITIVE half
  # maintained by hand. That is exactly the deep-deps transform, so it is the
  # transform's job now — each test carries a `DEPS` file and the real worker
  # mounts its deps, recursively deepened and shared by hash.
  #
  # ONE transform for the whole suite, not one per test: the workspace holds std
  # and every test's DEPS, so a dep reached by two
  # tests is one node computed once. What a test re-keys on is its own deepened
  # subgraph — narrower than the old lists, because a std entry now brings its
  # own deps (std/<e>/DEPS) instead of each test restating them.
  #
  # It is also the byte-identity guardrail the seeding needs: this is the REAL
  # deep-deps worker deepening the same entries `build-builtins.sh` hand-deepens
  # to form seed keys. If the two ever disagree, the seeded key stops matching
  # the key resolution forms and the tests that resolve that entry go red.
  #
  # NOTHING IS ASSEMBLED. The tree deepened here is the WORKSPACE ITSELF
  # (`/cas/args/in`), because the workspace now carries the root `.caos-expr`
  # this transform implements — `run std/deep-deps -- --in:@=.` — and every path
  # a `DEPS` file names is a real path in it. `tests/rgrep/DEPS` says
  # `../../std/rgrep`, and that resolves against the repo, not against a
  # look-alike the harness built out of build outputs.
  #
  # It used to assemble `{std/*, tests/*/DEPS}` from the build result. That
  # worked, but it made the DEPS paths a fiction: they were repo-shaped only
  # because the fabricated tree was made repo-shaped, and deep-deps was invoked
  # imperatively by a script rather than by the tree describing itself. Two
  # things had to land before this could be honest — `std/runner` becoming a
  # real entry, and the test fixture binary moving to `std/llm-stub` — because
  # a tree can only deepen itself when everything it declares is IN it.

  # THE DEEP-DEPS IMAGE — resolved the way its own `.caos-expr` resolves it.
  #
  # Two things stand in the way of just naming the entry as an image, and both
  # are load-bearing rules rather than accidents:
  #
  #   - A WORKER CANNOT EVALUATE. `resolve_run_image` reads a /cas path as the
  #     object it was made from, so the sentinel entry arrives as its own
  #     one-file tree ("image tree has no config.json"). Evaluation is a CLIENT
  #     capability, and it must stay one: `eval_path` on a `run` BLOCKS on the
  #     result, which is exactly what a worker may not do (design/map-then.md).
  #   - THE WORLD MUST MATCH. The build's own seed record holds a ready-made
  #     deep-deps image, but it is built from the TREE UNDER TEST — a `test`
  #     world binary, which the outer `host` server refuses to serve (measured:
  #     "caos world mismatch"). This stage runs in the host world, so it needs
  #     the HOST's deep-deps.
  #
  # Both dissolve the same way: `run docker://seeded-deep-deps -- --in:@=.` IS
  # `run-then` over the std entry with the sentinel as `--run`. That forms
  # precisely the arg-tree key the entry's expression forms, so the host's own
  # core-seeder-runner answers it with the host's image — no evaluator in the
  # worker, no blocking run, no world crossing. If the sentinel ever changes,
  # this forms a key nothing answers and the job fails loudly on the pending
  # timeout; it cannot quietly deepen with the wrong thing.
  caos get /cas/args/in/std
  caos get /cas/args/in/std/deep-deps

  # Each run ends a stage: a worker delegates its continuation instead of
  # blocking. So resolving the image ends THIS stage, `deepen` does the
  # transform, and `fanout` builds the wrappers. Everything a later stage needs
  # from here has to be curried through, because run-then passes only `--in`
  # and `--result`: `build` is what the build tool
  # returned (image, std, seed, time) and `ws` the workspace — all named
  # away from the reserved `in`/`result`.
  fwd=("--worker1:@=/cas/args/worker1" --stage=deepen
       "--build:@=/cas/args/result" "--ws:@=/cas/args/in"
       "--build-ws:@=/cas/args/build-ws")
  if [ -e /cas/args/api-key ]; then fwd+=("--api-key:@=/cas/args/api-key"); fi
  if [ -e /cas/args/only ]; then fwd+=("--only:@=/cas/args/only"); fi
  if [ -e /cas/args/test-salt ]; then fwd+=("--test-salt:@=/cas/args/test-salt"); fi
  deepen=$(caos curry /cas/args/image -- "${fwd[@]}") || fail "currying the deepen stage"
  caos run-then /cas/args/in/std/deep-deps -- --run=docker://seeded-deep-deps --then="$deepen"
  ;;

deepen)
  # `--result` is the deep-deps IMAGE the sentinel resolved to. Run it over the
  # assembled tree; `fanout` gets the deepened tree as its own `--result`.
  caos get /cas/args/result
  fwd=("--worker1:@=/cas/args/worker1" --stage=fanout
       "--build:@=/cas/args/build" "--ws:@=/cas/args/ws"
       "--build-ws:@=/cas/args/build-ws")
  if [ -e /cas/args/api-key ]; then fwd+=("--api-key:@=/cas/args/api-key"); fi
  if [ -e /cas/args/only ]; then fwd+=("--only:@=/cas/args/only"); fi
  if [ -e /cas/args/test-salt ]; then fwd+=("--test-salt:@=/cas/args/test-salt"); fi
  fanout=$(caos curry /cas/args/image -- "${fwd[@]}") || fail "currying the fan-out stage"
  caos run-then /cas/args/ws -- --run=/cas/args/result --then="$fanout"
  ;;

fanout)
  # The `then` of the deepening: `--result` is the DEEPENED tree, so each test's
  # deps are at `/cas/args/result/tests/<name>/DEEP-DEPS/<mount>`. `--build` is
  # the build tool's result and `--ws` the workspace (curried by stage3).
  caos get /cas/args/build
  if [ -e /cas/args/build/seed ]; then caos get /cas/args/build/seed; fi
  caos get /cas/args/ws
  caos get /cas/args/ws/tests
  caos get /cas/args/ws/tests/lib
  # Placeholders down to each test's mounts: `caos put` resolves a symlink into
  # /cas by RECORDED HASH, but the path has to exist to be resolved. One level
  # at a time — no mount content is fetched.
  caos get /cas/args/result
  caos get /cas/args/result/tests

  # The per-test map worker is curried HERE, where the image is a genuine
  # tree. Passing the image itself as a curried arg to a later stage
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
  map=$(caos curry /cas/args/build/image -- \
    "--worker1:@=/cas/args/ws/tests/lib/run-test.sh") || fail "currying the per-test runner"

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
  for d in /cas/args/ws/tests/*/; do
    t=$(basename "$d")
    if [ -n "$only" ]; then
      case "$only" in *" $t "*) ;; *) continue ;; esac
    fi
    caos get "/cas/args/ws/tests/$t"
    [ -e "/cas/args/ws/tests/$t/cli.sh" ] || continue
    mkdir -p "/tmp/sel/$t"
    ln -s "/cas/args/ws/tests/$t" "/tmp/sel/$t/test"
    if [ -n "$salt" ]; then printf '%s' "$salt" > "/tmp/sel/$t/salt"; fi
    # The seed records ride into EVERY wrapper (unlike the per-test std subset):
    # they carry the flake-builder image the inner seeder answers with, and any
    # test that builds a flake reaches flake-builder transitively. A symlink put,
    # so nothing moves; test-stack/worker seeds refs/caos/seed from it.
    if [ -e /cas/args/build/seed ]; then
      ln -s /cas/args/build/seed "/tmp/sel/$t/seed"
    fi

    # WHAT THIS TEST REACHES FOR, and nothing else — read back off the test's
    # own DEPS, which the transform above has already turned into a
    # DEEP-DEPS/<name> mount per line. The wrapper still hands the inner stack
    # `std`, which becomes the inner refs/caos/std, so all this does is place each
    # so all that happens here is routing each mount to the one its DEPS path
    # named. The CLOSURE is not computed here any more; deep-deps did it.
    #
    # The mounts are DEEPENED entries, which is what the inner stack should
    # receive: a `DEEP-DEPS/<dep>` mount is how a consumer reaches a std item
    # (design/caos-expr.md, the north star), and it is why these subsets carry
    # no std-root `.caos-expr` — they are already the transform's output, and
    # re-running it on them would only be an expensive identity.
    #
    # Undeclared is still UNAVAILABLE, deliberately, and the old rule still
    # says what an edge cannot reach for you:
    #
    #   THE GIT CLOSURE COVERS TREE REFERENCES AND NOTHING ELSE.
    #
    # A std entry's own `DEPS` edges now ride along (that is the transform), so
    # what a test must still name itself is only what is NOT a tree reference:
    #
    #   - A CURRY'S BASE — a BLOB holding the base image's hash. `deep-deps`
    #     resolves to a curry over the runner delta, so a test naming
    #     `deep-deps` still names `runner`.
    #   - A HASH BOUND AS A LITERAL. `rustc`'s seed result binds `--cargo=<hash>`
    #     that way, and `rustc` is a sentinel entry with no DEPS of its own — so
    #     anything built by rustc still names `cargo`.
    #   - A NAME THE SERVER OR CLIENT LOOKS UP. Running a raw flake tree makes
    #     the server resolve `flake-builder` BY NAME, and `std_image("bash")`
    #     resolves `flake-builder` by name — a nested mount does not satisfy
    #     either, so both stay named even where an edge also supplies them.
    #
    # And grep the CLIENT too, not just the test: `caos-cli talk` resolves
    # runner/bash-tool/llm-call/llm-step/rgrep/bash from constants in chat.rs,
    # and a Rust worker builds its path with `std_image("bash")`
    # (tests/commit) — neither shows up in a search for /cas/std in the test
    # directory.
    mkdir -p "/tmp/sel/$t/std"
    if [ -e "$d/DEPS" ]; then
      caos get "/cas/args/ws/tests/$t/DEPS"
      caos get "/cas/args/result/tests/$t"
      caos get "/cas/args/result/tests/$t/DEEP-DEPS"
      # A bash read loop, not sed/awk: this runs in the std/bash image, whose
      # flake carries neither (CLAUDE.md).
      while read -r dep_path mount; do
        case "${dep_path:-}" in "" | \#*) continue ;; esac
        case "$dep_path" in
          ../../std/*) ln -s "/cas/args/result/tests/$t/DEEP-DEPS/$mount" \
                             "/tmp/sel/$t/std/$mount" ;;
          *) fail "tests/$t/DEPS: $dep_path is not ../../std/*" ;;
        esac
      done < "$d/DEPS"
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
        ln -s /cas/args/ws "/tmp/sel/$t/workspace"
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
  then_img=$(caos curry /cas/args/image -- \
    "--worker1:@=/cas/args/worker1" --stage=summarize \
    "--build-time:@=/cas/args/build/time" "--start-time=$(date +%s)") \
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
