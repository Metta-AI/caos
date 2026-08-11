# Faster tests

A salted full run (`CAOS_SALT=$(date --iso=s) caos-cli run-tool test`) went from
**45s to 110s**. `build` is unchanged at ~9s; the tests are ~98s. This is what it
is and what the options are.

## Where the time went

The four slowest tests are exactly the four that resolve `llm-step`:

| test | time | resolves llm-step |
|---|---|---|
| llm-step | 52s | yes |
| merge-harness | 48s | yes |
| chat-offline | 46s | yes |
| caos-tools | 41s | yes |
| cargo-self | 31s | no |
| run-then | 30s | no |

`std/llm-step/.caos-expr` binds the tools it drives:

```
BASH_TOOL=curry DEEP-DEPS/bash-tool --
GREP=curry DEEP-DEPS/rgrep --
TOOLS=curry DEEP-DEPS/bash --
MERGE=curry DEEP-DEPS/merge --
```

Evaluating that RESOLVES all four, which means BUILDING all four: two rustc
compiles (bash-tool, and rgrep with its `regex` dep) and two flake builds (bash,
merge). Every consumer pays for every tool whether its turn calls it or not —
`caos-tools` drives one bash call and now compiles rgrep and builds merge.

Then the multiplier: **each test runs in its own stack with a private,
non-persistent redis**, so those builds happen independently four times over.

Two measurements that ruled out the obvious suspects:

- The `DEEP-DEPS` copy into each test's repo is **452K at worst, 3.8M across all
  29 tests**, and contains no copy of the runner binary (runner is a
  `{.caos-expr}` sentinel, not a delta). Not the cause.
- The engine is **already shared** — the inner runnerd delegates every worker to
  the outer engine through the granted socket. Containers already run on one
  docker. Redis is the only remaining isolation.

## Directions

### 1. Share the result cache across test stacks (the perf fix)

`stack/serve` hardcodes `REDIS_ADDR=127.0.0.1:$CAOS_STACK_REDIS_PORT`, on the
reasoning that "every placement puts the group in one netns, so there is no
address to negotiate". Sharing means an address knob plus pointing the inner
stacks at one suite-wide instance. The test container is already on `caos-net`
(it reaches `caos-registry:5000` by name), so it can route there.

The risk that looks fatal is not: `tests/cargo-self` is the one test that cares
about cold-vs-warm, and it already **salts to force a cold run** and asserts
comparatively rather than demanding zero hits — its own comment says a
`cache_hit:true` from redis "is pure timing". The other cache-aware tests assert
that an identical second run is a HIT, which sharing only makes more true.

Set expectations honestly: single-flight is per-server, so four tests starting
together will all miss and all build `rgrep` concurrently. This helps the
sequential case a lot and the simultaneous case not at all.

### 2. One stack for all tests (why it is not simply better)

`map-then` gives one container per child, and per-test JOBS are what buy per-test
caching and parallelism — "an unchanged test never runs at all" is what makes a
normal edit-and-test loop fast. One stack means one job: all-or-nothing caching,
and a warm suite goes from ~9s to a full re-run every time.

The real gap is that caos has no shared long-lived SERVICE across jobs. Sharing
redis gets the build reuse without giving up per-test caching, so it is the
cheaper trade unless that gap is worth closing for its own sake.

### 3. Bind less in llm-step (separate question)

Should a turn pay for tools it never calls? llm-step cannot resolve lazily —
a worker cannot evaluate a `.caos-expr` — so the choice is between the ENTRY
binding everything up front (today: correct, self-describing, eager) and the
CALLER binding what its turn needs from its own declared deps (cheaper, but the
caller has to know what llm-step wants).

This is a design question about where a tool set is declared, not a perf hack.
Worth deciding on its merits; the perf consequence is a reason to ask it, not the
answer.

### 4. Stop assembling per-test wrappers (simplicity, not speed)

`test.sh` routes each DEPS mount into a `std/` slot and builds a wrapper
`{test, std, seed, workspace?}`; `run-test.sh` then copies it apart into `./test`
and `./DEEP-DEPS`. Two copies, and hand-assembly of a shape the tree already
describes.

The deepened `tests/<name>` node IS `{cli.sh, DEPS, DEEP-DEPS/…}` — what the test
wants, with deps mounted under the names its own DEPS chose. One checkout of that
node could replace the routing and both copies (`cli_get` already produces
"ordinary rw files"). Key narrowing survives: deep-deps gives that node a hash
that moves only when the test or its deps move.

**Zero copies is not reachable**, and the blockers are not harness design:
materialized CAS content is read-only and owner-only; most of these tests mutate
(git init, build fixtures, commit, edit, re-run) and for several the mutation IS
the subject; and the CLI ingests git-tracked paths only. A writable tree is
forced. One checkout instead of assemble-plus-two-copies is the reachable win,
and it is worth ~nothing in time.

## Suggested order

1. Redis sharing — this is the regression.
2. The wrapper/checkout simplification — on its own merits.
3. The llm-step binding question — decide it as a design question.
