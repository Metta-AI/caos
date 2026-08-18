# Faster tests

A salted full run (`CAOS_SALT=$(date --iso=s) caos-cli run-tool test`) is **~26s**
against a warm stack — 33/33, `build` ~10s — from **103s**. The rest of this
document is how that was found, including the parts that were wrong, because
the things that mattered were not on anyone's list and most of the things on
the list were not measurable.

It got there in three passes, and the sections below are in the order they
happened rather than the order they matter: the shared stack and name
resolution took 103s to ~35s, then profiling `build` took the make stage 10.5s
to 6.5s, then profiling the test phase found the one that mattered — 29
identical image conversions per run — and took a real source edit from 54-99s
to 32.5s. Measured spread at the end of it: 24.8-31.8s over five consecutive
runs, so treat anything under a couple of seconds here as noise.

A fourth pass ("The longest test", below) went after the tail — chat-offline,
which was the whole `tests` phase on its own. It halved the longest test and
moved the total by nothing, because by then the total was not the tail's to
move. That is the section worth reading if you are about to optimise this
suite again.

A later split happened under different conditions: `llm-step` had grown from
one conversation test into four serial scenarios (core replay/interjection,
independent work, subagents, and Escape), while `chat-tools` still ran three
tool scenarios serially. They were again 12–22s tails. The focused
`llm-{step,async,subagent,interrupt}` and `chat-tools{,-mixed,-grep}` suites
give each behavior its own fixture and fan them out. On the same warm shared
stack, the seven selected tests take a 3s phase; with the image cache warm, a
fresh-`CAOS_SALT` full run was 41/41 in 14.9s (5s build, 8s tests). None of the
split shards took more than 2s under full-suite contention. This does not
overturn the lesson below: the measurement justified the split only after the
suite became tail-bound again.

## Read the timeline, not the total

The phase clock records absolute timestamps, so a run can be laid out as one
picture — and that picture is what found the last two thirds of the win. A cold
88s run was:

| | |
|---|---|
| build + the suite's own serial stages, before any test can start | +0 → +16s |
| the whole first WAVE of tests blocked on the shared stack's cold start | +16 → +44s |
| the tests | +44 → +88s |

Three separable costs, none of which a wall-clock total distinguishes: a 16s
ramp, a 28s cold start paid by every test in the first wave, and per-test setup
of 5.7s that later tests pay instead. Every earlier diagnosis in this document
was an attempt to infer that split from CPU samples, and each one was wrong.

What the split then made obvious:

- **The seed records were materialized by all 29 clients** — they carry the
  core image deltas, so they are most of a wrapper's bytes — although only the
  client that STARTS the stack has any use for them. Fetching them selectively
  took `args materialized` from 3242ms to 19ms.
- **And pushed by all 29**, to a stack seeded with them at boot. Asking first
  (one ls-remote, ~50ms) replaced a 1.7s no-op push.

Per-test setup: 5.7s -> 2.9s, and the suite 61s -> 35s.

## What actually did it

**Name resolution, not caching.** A name costs ~1s per lookup in a container
here — the resolver stalls on the AAAA half — and the answer is cached per
PROCESS, so git, which forks about ten helpers per operation, paid ten seconds
for every fetch and every push. Measured from a worker: `git ls-remote
http://caos-server` 10022ms, the same by address 50ms; `curl` 1046ms against
41ms; `curl -4` 8ms. A test job's setup was 83s, of which ~75s was this.
runnerd now resolves once and hands workers the address (setup: 83s -> 7.8s).

`/etc/hosts` cannot fix it: this image has no `/etc/nsswitch.conf` and no NSS
modules, so glibc never consults `files` — verified by adding both and watching
the stall persist. Addresses are the only lever.

**One shared test stack** instead of 28 private ones, which is what removed the
duplicated compiles: the sum of per-test times fell 385s -> 63s.

Neither was visible in the per-test times the suite reports, because
`run-test.sh` starts its clock AFTER setup. That is why `test-stack/worker` now
writes a phase clock into every test's record — the setup half of a test job
was unmeasurable, and three separate wrong diagnoses came out of inferring it
from `ps` instead.

## Naming the shared stack, and a build that is not quite reproducible

The stack is named after the image it runs, so a run reuses one rather than
starting another. Three ways of deriving that name were tried and all three
were UNSTABLE ON AN UNCHANGED TREE, in the same way: `caos hash <path>` and
`caos curry <path>` both INGEST materialized content, and materializing is not
identity — a git tree holds things a filesystem does not. Each ping-ponged
between two values, so clients split across two stacks and every other run paid
a cold start (34-39s reusing one, 60-106s starting another).

The name now comes from the ENGINE: the image id of the container the test is
running in. It is identical exactly when the image is, needs no filesystem
round trip, and says what it means — "the stack for the image these tests run".

Underneath that sat a real defect: **the build was not reproducible**. Two
builds of an unchanged tree produced different `caos` binaries, hence different
images, hence a new stack every run. The cause was incremental compilation in
the bake's persistent target dir, which makes rustc's output depend on what was
compiled there before; `CARGO_INCREMENTAL=0` in the build made two consecutive
builds byte-identical apart from the `time` field, which varies by design.

NOT FULLY CLOSED. With all of that, three runs in four reuse one stack and the
fourth still mints a second, so some residual nondeterminism remains
unidentified. The symptom is a slow run, never a wrong one.

## The build is not byte-reproducible, and that is what costs the cold start

`/bin/caos` differs between builds of an UNCHANGED tree. Two variants,
alternating. Since that binary is in `layer00` of every image, each differing
build mints a new test stack image, so the suite starts a fresh stack and
recompiles every std tool inside it: **35s reusing a stack against 67s minting
one**, which is the whole remaining gap.

It is worse than a test-speed problem. SPEC's premise is that work is
deterministic and never run twice; a compile with two outputs re-keys
everything downstream, so nothing below the build can cache across runs.

What it is, narrowed by diffing the two binaries:

- Every symbol is present in both, 7289 of them, at IDENTICAL sizes.
- What moves is ADDRESSES, the numbering of local `GCC_except_table` labels,
  and how string constants got merged. Layout, not semantics.
- The same 10 crates compile each time; only their ORDER varies.
- It survives `cargo --jobs 1`, so it is not cargo's scheduling.
- The same crate built repeatedly on the host — glibc AND musl targets — is
  byte-identical every time.

The obvious suspect was rustc codegenning units in parallel and emitting them
in completion order. IT IS NOT THAT — tried and disproved, which is worth
recording so nobody tries it twice:

- `[profile.dev.package.caos] codegen-units = 1` invalidates the ~176 baked
  dependencies (codegen-units is part of every unit's fingerprint, even scoped
  to one package): the build ran past TEN MINUTES against its usual eleven
  seconds.
- Setting it properly instead — `CARGO_PROFILE_DEV_CODEGEN_UNITS=1` in BOTH
  std/cargo/bake.nix's `deps` and its image `env`, so the bake and the per-edit
  compile agree — rebuilds cleanly (2m15s once) and does reach rustc: the
  compile log shows `-C codegen-units=1` on all 13 invocations.
- And the binary still alternates between the SAME TWO hashes as before, so
  codegen-units changes nothing here at all. Reverted.

So the cause is still unidentified. What is ruled out: cargo's scheduling
(`--jobs 1`), incremental compilation, codegen-units, and the host (the same
crate built repeatedly on the host, glibc and musl, is byte-identical every
time). What is left is something about the container build — the bake's
prebuilt artifacts, or the musl cross setup.

One observation not yet followed up: the variants correlate with machine load.
Under a quiet machine the last five builds went f69dbe91 then eab7abcf four
times — it settles. Every measurement here was taken on a box running suites
back to back, so this may be rarer in ordinary use than it looks.

`build.sh` records the compile in its result (`compiled`) — the env keys and
the crate list — because a worker's stderr is not kept for a job that
succeeded, and comparing those lists is what ruled out the crate set and the
ordering.

## What did not work, measured

- **Prebuilding std in `build`** — 31% less total work, and a wash, because the
  build is serial (below).
- **Sharing redis between private stacks** — unsound and useless (below).
- **More runner slots** — 8 to 20 changed the wall clock by 1s.
- **Resolving the REGISTRY's name** the same way as the server's: principled,
  ~1s per skopeo call, and no measurable difference across three cold runs
  (54/56s against 54/50/59s). Ten conversions are not on the critical path.
  Reverted rather than kept on the strength of the argument.
- **Resolver options** (`ndots:0`, `single-request`, `timeout:1`, `no-aaaa`):
  all still ~1012ms. Only an address, or `curl -4`, avoids the stall.

## The longest test: chat-offline, and what splitting it did not buy

After the three passes above, `tests` was 12s and **chat-offline was 12s of it**
— the next test was 5s. One test WAS the phase, so it looked like the obvious
last win.

It is not one slow thing. chat-offline drives the whole agent loop end to end,
seven times in a row: turn 1 with a bash sub-run, turn 2 on stdin, `talk`
sticky, `talk --new`, the inline file-tool batch, the mixed inline/bash queue,
and the grep fold. The LLM is the stub and answers in 0.0s, so essentially none
of it is the model. Warm, alone, ~8s:

| | |
|---|---|
| setup (fetch the stub, start the stub) | 1.30s |
| 7 turns of client-side work | ~2.6s |
| 7 turns of server-side job execution | ~4.1s |

Per turn, from the client's own phase events: `resolving the workers` 0.19s,
`pushing the turn` 0.08s, `the run` 0.30-0.93s, `fetching the turn` 0.08s. The
run time tracks job count exactly — ~15 jobs across the test, one per LLM round
plus one per bash/grep sub-run, at **~0.27s each**. That per-job figure is the
platform's floor, and a test that needs fifteen jobs pays it fifteen times.

**Two ways to see this that did not exist before.** The client already timed
every turn phase and then threw the numbers away — `run_cli_turn` prints a
`PhaseComplete` only above 1.0s, and every phase here is under it. Dropping
that threshold is a one-character edit and is how the table above was measured;
`the run` itself was not timed at all and now is. On the test side, `seconds` in
a record is one number for a dozen round trips, so tests/chat-* carry a
`stage()` clock that stamps `[+Nms]` into the record. Reach for those two before
re-instrumenting anything.

### The two things that were actually waste

**`std/llm-stub`'s build result was 45 MB to carry a 6 MB stub.**
worker-cargo's flat path staged *every* executable in the profile dir — but
that dir is the image's BAKED one, so each `--cmd=build` result also carried
all ten caos binaries, ~39 MB nobody asked for. The decomposed path already got
this right (`stage_fresh_binaries`, mtime >= job start); the flat path now does
the same. Fetching the stub: **735ms -> ~25ms**, and every `--cmd=build` result
tree-wide shrinks, including the ones rustc consumes.

**A flat `sleep 0.5` waiting for the stub to bind**, in all four stub tests,
when it binds in a few ms. Now a `/dev/tcp` probe, which also distinguishes
"died, retry another port" from "still coming up". Setup, with both: **1.30s ->
0.20s**.

### The split, and the honest result

At that point the rest was fifteen serial jobs, irreducible in place, so chat-offline became
three tests the fan-out can run at once — `chat-offline` (the `chat` verb),
`chat-talk` (the `talk` verb, seeding its own two-turn history), `chat-tools`
(inline / mixed / grep, which chain onto each other's trees and so stay
together). It did what it was supposed to:

| | before | after |
|---|---|---|
| longest chat test, in the suite | 12s | 7s |
| longest chat test, alone | 8s | 3-4s |
| the `tests` phase | 12s | 12-14s |
| a full `CAOS_SALT` run | ~26s | 23.6s |

**The phase did not move, and that is the finding.** The suite stopped being
critical-path-bound the moment chat-offline stopped being the tail. Sum the
per-test seconds of a salted run and you get **56s across a 14s span** —
effective concurrency ~4, against 8 slots — with the longest single test now
caos-tools at 7s. Neither the tail (7s) nor the slot arithmetic (56/8 = 7s)
accounts for a 14s span; the missing half is per-job SETUP, which `seconds`
excludes by construction (run-test.sh starts its clock once the stack is in
hand) and which two more jobs made two units worse.

So the split is kept on its merits — `--only=chat-offline` is 3s instead of 8s,
which is what you actually iterate against, and the tail is no longer one test
— but **it is not a suite win, and predicting one from "chat-offline is 12s of
a 12s phase" was wrong.** The next real win is per-job setup across 33 jobs,
not any single test. Measure the span against the sum before splitting anything
else: when they differ by 2x, the tail is not the problem.

## What it cost to get there

Four changes in a row were made without a measurement between them, and two of
them broke the suite in ways that presented as SLOW rather than failed: a
120s idle timeout that fired mid-suite, and a pre-seed inside the start lock
that turned a lost race into a stampede. Both were diagnosed as performance
problems before being recognised as bugs. The lesson is in the ordering — one
change, one measurement — not in any of the individual fixes.

The older analysis follows, and is kept because the reasoning it records is
still what the design rests on.

Numbers below are one 16-core / 60 GB host, several salted full runs plus
per-test `--only` runs. Take them as ratios, not constants: a run sharing the
machine with anything else inflates uniformly (one measured at 209s), so any
comparison has to be made against a baseline taken on the same idle machine.

## Where the time goes

Three phases, from `vmstat` across a run:

| phase | duration | CPU |
|---|---|---|
| ramp: build + the suite's own serial stages | ~16s | 55-90% IDLE |
| the fan-out proper | ~45s | 70-85% busy |
| tail: the last few long tests, alone | ~20s | 60-95% IDLE |

Average idle is 47%, which reads as headroom and is misleading — the headroom
is the ramp and the tail, and the middle is saturated.

**Slots are not the constraint.** The host runnerd defaults to 8, so 28 tests
run 8 at a time. Raising it to 20 leaves the wall clock unchanged — 85s against
84s — and roughly doubles every individual test's time. **Container launch is
not the constraint either**: 30 concurrent `docker run`s of the 494 MB stack
image finish in 3s.

**Contention is 3x.** Each test takes about three times in the suite what it
takes alone (chat-offline 46s/15s, caos-tools 39s/12s, llm-step 36s/11s,
rgrep 19s/5s). `ps` sampled through a run ranks the consumers:

    rustc  2731     caos  1412     git  823     ld  511     git-remote-http  472

rustc first by a factor of two, then the object movement feeding it.

## What is duplicated

Flake-built entries are already shared and cost nothing: the flake-builder
short-circuits on a content-keyed registry tag (`flake-<H>`, `deps-<D>`) and the
registry is deliberately shared across test stacks (stack/serve: "a
content-addressed registry shared across tests cannot leak behaviour, only
work"). `merge` resolves in 2s alone despite "building" a nix flake.

**rustc results have no such short-circuit.** A rustc-built entry resolves to
`curry(runner, worker1=<binary>)` — a git object memoized only in the private
redis of the stack that built it. So every stack that names one compiles it
again:

| entry (rustc-built) | stacks that compile it |
|---|---|
| bash-tool | 5 |
| llm-stub | 5 |
| rgrep | 4 |
| llm-step + llm-client | 4 |

`std/llm-step/.caos-expr` binds `bash-tool`, `rgrep`, `bash` and `merge`, so
resolving llm-step at all compiles all of them, in each of the four stacks that
name it, whether or not the turn calls the tool. That is the 45s -> 103s
regression: the same work, multiplied by the number of stacks.

And it lands at the HEAD of the longest serial chains — nothing in
chat-offline's conversation starts until llm-step's whole tool set is built.

**The other duplication is the stacks themselves.** Every test brings up redis,
a server, a runnerd and a seeder, and git-fetches its deps and the seed records.
None of that shows in the reported per-test times: `run-test.sh` starts its clock
after the stack is up. Twenty-eight of them.

## Two fixes that do not work

### Sharing redis across test stacks

Wrong twice over.

**A cached result is a hash, and the objects are not shared.** The value under
`caos:result:<arg-tree>` is `"<type> <hash>"`; the objects live in the repo of
whichever server ran the job, and each stack's repo is private
(`/tmp/stack/git`, seeded per job from its declared deps). A stack that hits
another's entry gets a hash it cannot resolve. This tree already paid for that
lesson in another guise — `tests/lib/run-test.sh`: "The client has the hash and
not the objects, so the push dies on the first one it cannot read."

**And it would not help.** Single-flight is per-server, so concurrent misses all
build. The four llm-step tests are all long and all start in the first slot
generation. Confirmed by the 20-slot run, where all 28 start at once.

(Two of redis's three key spaces ARE safely shareable — `caos:layer:` and the
image-conversion key name content in the shared registry, not in a private repo.
That is a smaller prize than the compiles.)

### Prebuilding std in the build stage

Implemented and measured, on the same idle machine:

| | baseline | prebuilt std |
|---|---|---|
| build | 9s | 22s |
| tests | 90s | 79s |
| total | 103s | 106s |
| sum of per-test times | 385s | 264s |

It works — 31% of the total work removed, llm-step 23s -> 5s, caos-tools 47s ->
16s, bash-tool 6s -> 2s — and it is still a wash, because the build is SERIAL
and everything waits for it. Moving duplicate work onto the critical path is not
the same as deleting it.

Two things learned in the attempt, both of which outlive it:

- Running the entries CONCURRENTLY to hide the cost makes it worse: each client
  re-ingests the same workspace and re-pushes the same objects, and two clients
  pushing one object race on its content-addressed ref (below). Overlapping
  builds the server was already single-flighting is not worth six redundant
  ingests.
- The evaluator is the only honest source of a cache key. `eval-path --records`
  writes the `(arg-tree, kind, result)` of every run an evaluation performed —
  the key and the value, formed by the same code a consumer will form them with.
  A hand-assembled key that drifts does not fail; it silently matches nothing.

## The shape: ONE test stack, shared by every test

`test.sh`'s fan-out becomes a `map-then` over TWO inputs with a trampoline map
worker (one map image, so the children differ by CONTENT — each carries its role
and the trampoline runs it):

- **A, the stack.** Brings up one test stack and stays alive.
- **B, the tests.** Waits for A, then `map-then`s over the 28 tests as today,
  every one of them pointed at A.

Why this is the fix and cache-sharing was not:

- **Single-flight starts working.** It is per-server; with one server, four
  tests wanting rgrep collapse into ONE run even when they start together —
  precisely the simultaneous case sharing redis could not help.
- **The missing-object problem cannot arise.** One repo, so a hit's result is
  always resolvable.
- **The duplicate work is deleted, not relocated.** It happens once, inside the
  fan-out, in parallel with everything else — not on the critical path.
- **Per-test JOBS survive**, so per-test caching and parallelism survive. "An
  unchanged test never runs at all" is what makes the warm loop ~9s, and one
  stack per SUITE would have cost exactly that.
- It deletes 28 servers, 28 redises, 28 runnerds and 28 seeders. The test
  containers become plain clients — no inner stack, no engine socket.

**It needs no new caos concept.** The service is a sibling job that happens to
stay alive; there is no runnerd service registry, no host-side lifecycle, and
the tool contract (same ArgTree for both callers) is untouched.

### Discovery and lifetime: a ref, named per run

A and B are separate containers. Workers get their own netns on `caos-net`
(which is what lets a test stack bind its own `:80`), so A has a real address
there, and the only mutable namespace both can reach is the outer server's git.
So A publishes its address to a ref and B polls until it appears; B's `then` —
the summariser — writes the stop signal to the same ref, and A exits.

That works because **a promise does not complete until it resolves**: B's
`map-then` leaves a continuation, and the server does not finish B until the
whole nested fan-out has. So "the map's children are done" really does mean the
tests are done. A needs a timeout anyway: a failed subtree must not hang it.

**The ref name carries a per-run token**, minted BEFORE the fan-out and passed
into both A and B — one mint, threaded down, rather than either child deriving
its own. Two reasons, and the second is not optional:

- Two suites running at once (two tui conversations) must not share a ref.
- **A MUST NOT CACHE.** A cached stack-starter returns a result without starting
  a stack. The token in its args is what prevents that — and it has to cover B
  as well, or the bad case is B cached and A not: a stack comes up that nobody
  uses, waiting for a stop signal that never comes.

Neither A nor B caching is fine, and consistent with what is already here: the
`summarize` stage never caches either (a timestamp in its args). The per-test
grandchildren — the jobs whose caching actually matters — are unaffected.

Note what minting costs: the stage that mints the token stops caching, and so
does every stage the token is threaded through. So mint it at the LATEST point
still upstream of both children — no earlier than it has to be.

### Persistence between runs

Worth having, and the rule is: **the persistent data dies whenever the stack
does.** Not for cache correctness — results are keyed by arg tree, which
includes the image, so entries from another tree are unreachable rather than
wrong — but because the stack is the thing UNDER TEST. Data written by a server
build with a bug in it is not data a later run should inherit.

That rule falls out of keying the storage on the stack image digest: a changed
tree is a changed image is a fresh store. Old ones are then a disk-space
question (prune by age, as the host stack already does), not a correctness one.

This is the one piece needing a new mechanism, because a worker container has no
persistent storage: runnerd would mount a named volume, keyed on the image
digest, for an image that ASKS for one in its config — the same shape as the
existing `CAOS_GRANT_ENGINE_SOCKET` grant, and for the same reason (only an
image's author can ask, never a job or its args). So it is Phase 2; Phase 1 keeps
the data in A for the duration of one suite and still gets everything above.

### Two things that will bite

**`ensure_pushed` races, and here it becomes load-bearing.** Two clients pushing
the same object both plan a create — the ref was absent when receive-pack read
the advertisement — and the loser dies with "cannot lock ref …: reference
already exists". `--force` does NOT fix it (the create precondition comes from
the advertised state, not the refspec); the fix is to retry that one error.
Today nothing hits it because every test has its own server. With 28 clients on
one server it is routine, so it must be fixed FIRST.

**`CAOS_STUB_HOST` breaks.** `run-test.sh` sets it to `127.0.0.1` because
"siblings share this container's netns, so localhost is the stub's address".
With a shared stack, A's runnerd launches workers into A's netns, not the test's,
so every stub-server test (chat-offline, chat-talk, chat-tools*, llm-*,
caos-tools, merge-harness, llm-call, max-tokens) loses its stub. The test container has its own `caos-net` address, so
the fix is for the stub to bind `0.0.0.0` and `CAOS_STUB_HOST` to be that
address — but it is not free, and it will present as a mystery failure if it is
not done up front.

### What does NOT break

The cache-sensitive tests were already written for a warm, shared cache. They
defend with salts, not with private stacks:

- `cargo-self`: "`ws` … does not change between suite runs, so without one the
  'cold' run is served from the PREVIOUS suite's cache … That is why this test
  has been flaky."
- `rust-worker`: salts each source per run "so `first-run` is always a genuine
  cold path, never a cache hit from a previous run".
- `file-count`, `dirs-only`: "Both uncached (fresh salt per tests/run.sh)."

Nor does the DEPS discipline weaken when A holds all of std: "undeclared is
unavailable" is enforced by what `run-test.sh` copies into the TEST's repo (the
CLI ingests git-tracked paths), not by what the server happens to hold.

## Order

1. Retry the `ensure_pushed` create race. Required by everything below.
2. Phase 1: the two-child fan-out, ref rendezvous, per-run token, stub-host fix.
3. Phase 2: the persistent store, keyed on the image digest, behind an image
   grant.
The prebuilt-std experiment is REVERTED, not carried: once the stack is shared
it is strictly worse — the same work, on the critical path. `eval-path
--records` went with it. Both are described above rather than kept in the tree,
because the measurement is what was worth having and the code was a means to it.
The seeded-sentinel dispatch fix below is the one part that stands on its own
and stays.

## Adjacent bug, found and fixed on the way

A `docker://seeded…` job could be claimed by a GENERIC runner, which can only
`docker run seeded-deep-deps` and die. `offer_job` prefers the most specific
parked poll, so this needs the seeder not to have parked yet — a real window,
since a stack that publishes its seed AFTER boot answers nothing until the
seeder's next 5s rescan. Observed from `build.sh` publishing std and resolving
it moments later, and the error points nowhere near the cause.

A sentinel now defers generic polls for its whole pending window, which makes the
contract what it always claimed to be: it waits for an answerer, and fails on the
pending timeout if none comes. The seeder's 5s `RESCAN` is worth dropping to
~500ms behind an mtime check on the ref (a synchronous `reload` was considered
and rejected: the seeder cannot observe its own readiness, because parking
happens inside a long poll, so an honest one needs a control channel on the
seeder AND a server endpoint exposing parked polls — two new surfaces to save
seconds that the defer already makes harmless).

## Profiling `build` itself

`run-tool build` with a fresh salt was ~12s on a machine that `/proc/stat` says
was 90% idle throughout, peaking at 53% of 16 cores for about two seconds. So
the question was never "is the compile slow", it was "what is the other 9s".

The answer is now readable from the result. `ts` in `caos-tools/build.sh` and
`bts` in `build-builtins.sh` write to ONE file (`CAOS_PHASES`), and the merged
clock lands in the result tree's `compiled` blob. A build's cost survives the
container it ran in.

Where a 10.6s build goes:

```
0.6s   client: reduce the tree, ingest, submit
2.5s   five pre-`make` job round-trips (~0.5s each: dispatch, podman
       create+start, fetch args, run, put result, register the next)
6.5s   the `make` stage:
         3.3   cargo build --workspace   (critical path: caos rlib 1.2s
                                          -> caos-cli 1.8s; server 2.1s runs
                                          alongside)
         0.1   stage binaries into a nix-shaped layout
         0.2   seed stack up
         3.0   build-builtins.sh:
                 0.3  stage worker layers      (git add ~40 MB of binaries)
                 0.3  three git-docker deltas  (registry hits)
                 0.5  stage 13 std sources
                 0.2  hand-deepen 13 entries
                 1.1  the first `caos curry`   <- see below
                 0.3  the second curry, mktree, push refs/caos/seed
         0.1   assemble and put the result tree
0.4s   client: finish
```

Four things were fixed by reading that:

- **Polls that stepped by a second.** `stack/serve` waited for the server with
  `git ls-remote` on a 1s step and for runnerd with an unconditional `sleep 1`;
  `build.sh` waited for `/tmp/seed-ready` on a 1s step. All three answer in tens
  of milliseconds. Seed stack up: 2.0s -> 0.2s.
- **A name lookup per process.** `CAOS_REGISTRY_HTTP` was `caos-registry:5000`;
  a lookup costs ~1s in a container here (the resolver stalls on the AAAA half)
  and is cached per PROCESS, so every curl and every skopeo paid it again. It is
  now the address `CAOS_SERVER_URL` already carries, verified with curl, with
  the name as fallback. Bootstrap: 4.4s -> 3.6s.
- **zlib on a throwaway repo.** Bootstrap writes ~34 MB of freshly compiled
  binaries as loose objects and pushes ~13 MB of pack into a server created
  empty seconds earlier. `CAOS_CLIENT_REPO_THROWAWAY=yes` turns compression off
  for that client; caosd's persistent one is deliberately excluded.
- **A curl probe in a script that also runs where curl does not exist.** Not a
  saving — a self-inflicted outage, recorded because it cost a debugging round.
  `stack/serve` runs in the seed placement too, whose image is the BUILDER, and
  that flake has no curl: exit 127 on every attempt, the loop runs out, and
  every seed stack dies "server never came up". It presented as a slow, failing
  build. The probe stays `git ls-remote`.

11.9s -> 10.6s, and the `make` stage 10.5s -> 6.5s.

### What is left, and why it is not taken

- **The 1.1s first `caos curry`.** `ensure_pushed` ships 13 MB into the seed
  server, which is created empty and destroyed seconds later, while the OUTER
  server already holds those objects — its own `seed-built` push takes 30ms.
  The obvious fix is to point bootstrap's client at the outer server and drop
  the inner stack entirely. DELIBERATELY NOT DONE: the seed stack runs the
  server binary this build just compiled, and is the first thing that exercises
  it. Bootstrapping against the host's older server would build the new core
  with the old code — exactly the "we are also testing changes to the stack"
  case this design is careful about elsewhere.
- **~1.0s of bootstrap that needs nothing from the compile** (staging 13 std
  sources, the hand-deepen, the registry deltas) runs strictly after a 3.3s
  compile that leaves half the machine idle. Overlapping it means a prestage
  entry point in `build-builtins.sh` plus a results cache for the two loops —
  real structure for ~10% of a build.
- **The compile's 3.3s is a chain**, not a width problem: `caos` rlib 1.2s then
  `caos-cli` 1.8s. Nothing to parallelise without splitting the `caos` crate;
  the linker is the other suspect and changing RUSTFLAGS re-bakes all ~176
  dependencies, which is what made the codegen-units experiment above expensive.

## Profiling the test phase

`run-tool test` was 50-68s where a re-run minutes later was 18s, and the machine
sat at 1-3% busy for a ten-second stretch in the middle. The per-client `phase`
clock (copied into every test's record as `phases.log`) says where, and all four
answers were waits, not work.

**The stacks were dying two minutes after every suite.** The `stack` role reads
`CAOS_TEST_STACK_IDLE_SECS` and defaults it to 900. The `docker run` that starts
that role ALWAYS passes the variable, and defaulted it to 120 — so the 900 was
dead code and every stack idled out about two minutes after the run that created
it. Any next run paid a full cold start. That is what "consistently 50 seconds"
was. The two literals now agree, and must stay agreed.

**A dead address costs 2.1s per probe, and was probed six times.** An idled-out
stack's published address is an address on caos-net with nothing behind it, so a
SYN is dropped rather than refused and `git ls-remote` sits in libcurl's connect.
`stack_alive` asks three times — 11.3s — and a cold suite called it twice: once
on the way in, and again in the wait loop, which re-read the same ref, got the
same dead address, and probed it afresh. 23s of a 68s run, at 1% CPU:

```
     0   start
   183   addr: fetch done
 11438   alive: checked http://10.89.0.108 rc=128      <- 11.25s
 11792   addr: fetch done                              (same address)
 23086   alive: checked http://10.89.0.108 rc=128      <- 11.25s again
 24209   stack found (http://10.89.0.125)
```

A `timeout 1 … /dev/tcp` gate in front of the ls-remote bounds the dead case at
1s and costs nothing live (a listening socket accepts from the backlog even when
the server behind it is saturated), and the wait loop now remembers the address
it just buried and only probes a CHANGED one. The same trace is 3.4s once.

**Every client fetched 55 MB of seed to use none of it.** The seed closure is 50
objects: 23 trees under a kilobyte, and 27 blobs totalling 55.1 MB — 32.7 MB of
which is one copy of the caos binary. All 29 clients fetched the lot from the
outer server (2.8s warm, 5-6s under eight-way contention) and 28 of them then
found the stack already seeded and pushed nothing. That is the git and server CPU
that was visible in `top`.

Skipping the fetch when the stack is already seeded DOES NOT WORK, and was tried:
`$SEED` is what run-test.sh points a test's own repo at as an ALTERNATE, and the
test's objects reference these trees — `rust-worker` dies with `fatal: bad tree
object <oid>`. But it is only the trees that are wanted. `--filter=blob:none`
fetches the shape and none of the payload: **116ms and 92 KB**, measured
in-container, against 2.8s and 55 MB. The blobs are what the SEEDER hands to a
job, which is the inner stack's copy, pushed by whichever client found the stack
unseeded — the one client that still fetches them in full. Per-client setup:
2.8-5.9s -> 0.63s.

Measured, all 29 tests forced to rerun (`--test-salt`):

| | before | after |
|---|---|---|
| cold stack (address stale) | 68s | 26-28s |
| warm stack | ~50s | ~31s |
| nothing to rerun | 18s | 18s |

### What is left

- **~0.5s per job of engine latency.** The slowest client in a run spends 4.1s
  between `args materialized` and its first ref read, which is `docker inspect`
  over the granted socket while podman is creating eight containers at once.
  Nothing in this tree can make podman faster; running fewer, longer jobs could.
- **Eight runner slots** means 29 tests run in four waves. The waves are visible
  in the container event log and are the reason a 0.6s setup still costs ~2.5s
  of suite time.

### The one that mattered: 29 conversions of one image

Everything above was measured against an UNCHANGED stack image, which is not
what an edit-build-test loop does. A source edit moves every binary, so the
stack image is new, and that path measured 54-99s where an unchanged image was
~28s. The container event log for such a run has a 24-second hole in it with no
containers created at all:

```
   0s  #####     creates=4
   8s  #         creates=1
  12s            creates=0     <-- 24 seconds of nothing
  36s  #########  creates=9
  42s  ########################  creates=21
```

`convert_git_image` and `ensure_layer` cache in redis: read the key, and on a
miss do the work and write it back. That is CHECK-THEN-ACT, and the server
answers on a thread pool. All 29 clients ask for the same new image in the same
moment, all 29 miss, and all 29 materialize a ~200 MB tree to a temp dir, tar
it, sha256 it and push it. The log says it outright — 29 identical
`converted image <hash> -> sha256:…` lines, one per test — and it is the
skopeo/gzip CPU visible in `top` during a run.

Both are single-flighted now, behind an in-process per-key lock with the cache
RE-READ after acquiring. One process per stack, so there is nothing to
distribute and no lease to expire.

`build && test` after a real source edit — every test re-keys, image is new:

| | |
|---|---|
| before | 54s, 62s, 75s, 89s, 99s |
| after  | 32.5s, 32.5s |

### Storage, which was NOT the problem

Worth recording because it looked like it was. The suite leaks: podman had 252
images / 92 GB (99% reclaimable) and the registry 244 GB of blobs across 166
tags, with nothing collecting either, on a disk at 91%. Pruning podman freed
65 GB and changed the suite time by nothing measurable (27.5s -> 28.6s on the
cached path). The server's git CAS is likewise unbounded — 2255 refs, 116k loose
objects, 5 packs, no repacking by design — and a full ref advertisement over it
still costs 42ms. Neither store explains any of the time. The leak is still real
and still unaddressed; it is a disk problem, not a latency one.

Also a red herring, recorded so it is not chased again: a raw TCP connect from
the HOST to a container IP takes a consistent ~1020ms (the first SYN is dropped
and retransmitted). Container-to-container is ~20ms and the host CLI talks to
the published port on localhost at 1-2ms, so nothing in the suite pays it — only
diagnostics that dial the container address directly.
