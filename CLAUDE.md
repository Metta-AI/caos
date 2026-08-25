Read ./SPEC.md

# Shell, under `set -euo pipefail`

Every script here runs with it, and two constructs quietly break under it.

- **`[ cond ] && action` exits the script when the condition is false**, if that
  list is the last command in its scope (a function, a loop body, a `{ }` block,
  the script). Write `if`. This is not hypothetical: it is the single largest
  source of bugs in this tree's shell — `build-builtins.sh` still carries three
  latent instances, one of which makes `./build-builtins.sh <name>` exit 1
  before doing anything.
- **`pipefail` makes a pipeline fail on its LEFTMOST failure**, not its last
  command. `curl -f ... | awk` returns curl's 22 on a 404, so a lookup whose
  "absent" answer is a 404 dies instead of returning empty. Distinguish absent
  from broken explicitly — a swallowed error here silently hides an unreachable
  service.
- **A comment inside a backslash-continued command severs it.** An env prefix
  written one-assignment-per-line (`FOO=1 \` … `bash serve &`) breaks the
  moment a comment lands between two of those lines: the continuation joins
  INTO the comment and the command runs with none of the environment. It is
  not a parse error, so `bash -n` passes and the damage shows up far away —
  in `test-stack/worker` it was a stack dying at bring-up with "serve needs
  CAOS_STACK_STATE", 28 clients polling for an address that never appeared,
  and a suite that read as SLOW rather than broken. Put the comment above the
  block.
- **`${VAR:?message}` is word-split unless you quote it.** An apostrophe in
  the message (`${X:?the socket's path}`) opens a single-quoted string that
  never closes and swallows the REST OF THE FILE; bash then reports an
  unterminated `if` hundreds of lines away, nowhere near the cause. Write
  `X="${VAR:?message}"` — inside double quotes the message is literal.
- **A worker script only has what its image's flake lists.** `std/bash` is
  bash, coreutils, diffutils, gnugrep, findutils and jq — there is **no
  `sed`**, and no awk. `sed 's/^/  /'` in `caos-tools/test/worker.sh` passed two
  green suites before it fired, because it only ran on the failing-test path:
  the report that exists to explain a failure was the thing that broke. Prefer
  a bash loop (`while IFS= read -r line`), and when reaching for a binary,
  check `std/<image>/flake.nix` first — an absent one is exit 127 at runtime,
  not a build error.

# Nix

- **Everything an image's `contents` puts on disk is a SYMLINK into
  `/nix/store`.** `dockerTools` lays contents down with `lndir`, which mirrors
  directories and symlinks files. So `cp -R` out of an image layout copies
  links, not content, and the copy is read-only and store-shaped. Use `cp -RL`
  — `build-builtins.sh` learned this for published flake trees, and `serve`
  learned it again for the seeded git dir, where a symlinked `HEAD` made git
  call a perfectly good repo "not a git repository" (`validate_headref` accepts
  a symlinked HEAD only if the target starts with `refs/`) while gix opened it
  and the server started anyway.
- **`runCommand name {} '' … '' + s` parses as `(runCommand name {} '' … '') + s`.**
  That concatenates the DERIVATION with the string, so you get a store path
  with your script text glued to the end — and it evaluates, builds, and fails
  much later with something like `<store-path># my comment: No such file or
  directory`. Parenthesize the whole script when appending to it.
- **`nix build` with TWO installables and TWO `-o` flags does not create both
  symlinks.** `nix build .#a -o result-a .#b -o result-b` leaves one pointing at
  a previous build, and the other can be missing entirely — so you read a stale
  binary and blame the code. Build one output per invocation. (`--refresh` is not
  needed: nix picks up dirty-tree edits fine.)

# Workers

- **Workers are NOT network-free.** This gets asserted over and over and it is
  wrong. A worker container is launched with `--network caos-net`
  (`rust/crates/runnerd/src/main.rs`) and has to be — it reaches the server over
  HTTP. Beyond that, `std/llm-client` posts to `https://api.anthropic.com`, and
  `std/flake-builder`'s worker runs `nix build` (fetching flake inputs from the
  internet) and `skopeo copy` to the registry. A worker that wants the network
  has it.
- **What is genuinely client-side is `:@@=` RESOLUTION, and the reason is that
  the KEY MUST BE CONTENT, not a name.** Not determinism: a locator carries a
  mandatory full commit sha, so `url + rev` is deterministic and could be
  resolved anywhere. The point is that the ArgTree *is* the cache key, so the
  locator has to become an oid before the request is formed — otherwise the URL
  sits inside the key and two consumers pinning the same rev through different
  URLs (a fork, a mirror, ssh vs https) get different keys for identical
  content. Mechanically, the worker's `HttpTransport` also has no git repo or
  remote to fetch INTO, which is why `Transport::fetch_git_ref` defaults to
  `Ok(None)`. Neither of those is a sandbox.
- **Where "no network" is true it is a BUILD-level choice.** `std/cargo` builds
  `--offline` against a vendored registry (`std/cargo/bake.nix`,
  `vendorCargoDeps`) — which is why a crates.io dep missing from the bake anchor
  fails instead of being fetched (`tests/lint/lint-bake-anchor.sh`). That is the build
  refusing to reach out, not the container being unable to.

# Git

- **A `git fetch` can fail over an object it never asked for.** The post-fetch
  connectivity check is `rev-list --not --all --alternate-refs`, so it walks the
  tips of every ALTERNATE object store too — and any fetch into such a repo can
  die with `missing blob object <x>` naming an object with nothing to do with
  the fetch, blaming the fetch. It cost a session on `tests/remote-ref` back when
  the harness gave each client repo an alternate: the identical `git fetch`
  succeeded in any ordinary repo and failed in that one, so the difference was
  the alternate, not the command. Add `-c core.alternateRefsCommand=true` when
  the fetched closure stands alone (`:@@=`'s `--depth 1` fetch does); do NOT add
  it where the alternate's tips are legitimately part of the history you are
  completing. **The test harness no longer creates one** (`tests/lib/worker`
  says so, and says to look there first if a push ever dies unable to read an
  object), which is what lets `tests/push-closure` reproduce at all — it used to
  have to delete the alternate itself.

- **An unsalted `run-tool test` does not prove a push works.** `ensure_pushed`
  asks before pushing — a `HEAD /object/<argtree>`, and on a hit it returns
  without running git at all — so re-running with an unchanged tree does not
  push, pack or traverse anything, and any defect in forming or packing the
  request is invisible. (It was invisible before that probe too: the push was a
  no-op update of a ref already at that hash.) Only a NEW ArgTree builds a real
  pack — which is why the primary gate is
  `CAOS_SALT=$(date --iso=s) result/bin/caos-cli run-tool test` (SPEC.md) and
  why a green unsalted suite once sat next to a hard-failing salted one for a
  whole session. Run the salted form before believing a push-path change.

# Caches and defaults, when a suite fans out

Both of these presented as "the tests are slow", cost a long time to find, and
are the kind of thing that is invisible until 29 clients arrive at once.

- **A redis cache read followed by a redis cache write is CHECK-THEN-ACT.** The
  server answers on a thread pool, so a fan-out means every client misses the
  same key in the same instant and every client does the whole job. Measured:
  29 tests produced 29 identical `converted image` lines, each materializing a
  ~200 MB tree to a temp dir, tarring it, hashing it and pushing it — 24 seconds
  in which the suite started no containers. Single-flight anything expensive
  behind a per-key lock and RE-READ the cache after acquiring it. There is one
  server process per stack, so an in-process lock is the whole requirement.
- **A default is dead if every caller passes the variable.** `test-stack/worker`
  read `CAOS_TEST_STACK_IDLE_SECS` with a 900s default, and the `docker run`
  three hundred lines away that starts that role always passed the variable,
  defaulting it to 120. The 900 was decoration; every shared stack died two
  minutes after the suite that created it and every later run paid a cold start.
  When a value is read in one place and supplied in another, the SUPPLIER wins —
  grep for the variable, do not read the default and believe it.
- **A dead container address does not refuse connections, it swallows them.**
  Nothing listens on an IP whose container is gone, and the bridge drops the SYN
  rather than sending RST, so anything that dials it waits out a TCP retransmit
  — `git ls-remote` sat there for 2.1s, three retries made 11.3s, and the caller
  did that twice. Bound any probe of an address that might be stale
  (`timeout 1 bash -c "exec 3<>/dev/tcp/$host/$port"`), and do not re-probe an
  address already proven dead — wait for it to CHANGE.

- **An idle machine mid-run means a job is waiting, and a `docker://seeded…`
  job is the one that waits in silence.** A seeded sentinel defers generic
  runners for its whole pending window (900s in a stack), so when its key
  doesn't match the seed record NOTHING happens — no container, no log line —
  and the eventual `no runner for arg_tree …` blames capacity, the one thing
  that isn't wrong. The server now proves this instead: a parked poll whose
  `required["base"]` equals the job's `base` IS that sentinel's seeder, so a
  disagreement is permanent, and after `CAOS_SEEDED_GRACE_SECS` (45s) the 503
  names the differing args. **Read that message rather than re-deriving it** —
  it prints both oids, and the fix is always one of the two sides being stale.
  Anything you hand-roll that forms a seeded key (a `caos run-then` against a
  sentinel, in `caos-tools/*/worker.sh`) must match `build-builtins.sh`'s record
  exactly, `strip_caos_expr` included; nothing else checks that agreement.

# Before committing

- **Never read a gate's exit status through a pipe.** `cmd 2>&1 | tail`
  reports `tail`'s status, so a failed command looks like a pass — that happened,
  and the next step ran against a stack that was not up. Use
  `cmd 2>&1 | tail; echo "EXIT=${PIPESTATUS[0]}"`, or don't pipe.
- **`run-tool test` does not cover `nix build`.** The suite compiles the tree
  with cargo over the real `rust/crates/` directory; `nix build` compiles a copy
  filtered by the flake's `src`. Anything that filter drops is invisible to a
  green suite — four `include_str!("githist/*.sh")` calls merged under a
  passing run and broke `nix build` on arrival. `tests/lint` now runs
  `tests/lint/lint-flake-src.sh` for the embedded-file case; for anything else the
  filter touches, run `nix build` yourself before committing.
- **`caosd up` does NOT get a new `caos` binary into worker images —
  `caosd reset` does.** A worker image's `/bin/caos` is copied in by the
  flake-builder at IMAGE-BUILD time (`std/flake-builder/worker`:
  `cp /bin/caos "$l/usr/bin/caos"`), and the flake-builder is reached through a
  seeded sentinel whose ArgTree is `{base: docker://seeded-…, in: <std entry
  tree>}` — the binary is nowhere in that key. So a rebuilt binary leaves every
  key unmoved, redis answers from the memo, and the OLD image is handed out
  however many times you re-run `caosd up`. Reproduced deliberately: a marker
  compiled into the worker's usage banner, `nix build && caosd up`, and the
  bash image came back at the same oid with no marker, while `server.log`
  showed `cache hit: arg_tree=… -> tree …` naming the previous deploy's image
  and the fresh seed record naming a different one.
  This bites only when something calls a NEW VERB on the DEPLOYED (outer)
  stack — `caos-tools/*` do, the suite does not, because it compiles its own
  binaries inside the test stack. The symptom is a worker dying with a plain
  `caos: usage:` listing that is missing the verb you just added.
- If this doesn't catch everything, we need to add it to the above step


# Tests

- **A test is an ENTRY, not a script the harness interprets.** `tests/<name>/`
  carries a `.caos-expr`, a `DEPS` and a `worker.sh`, exactly like a std entry,
  and running a test is EVALUATING it (`dev/run-test` does nothing else, plus
  turning the outcome into a verdict). Two consequences worth internalising:
  - **Naming a dependency resolves it.** `:@=` evaluates any tree carrying a
    `.caos-expr`, not just `--base` (caos-eval's `eval_if_evaluable`), so
    `--rustc:@=DEEP-DEPS/rustc` hands the worker the BUILT image. A worker
    cannot evaluate — its `caos` has `eval-path-then` and no `eval-path` — and
    with this it never needs to.
  - **A client test asks for its worktree.** `:@=` ingests git-tracked paths, so
    a test that drives `caos-cli` names `--base:@=DEEP-DEPS/lib` and DEPs on
    `../lib`; `tests/lib`'s `/worker` stages the repo and then runs `worker1`.
    Grep `DEPS` for `lib` to see which tests are client tests. A worker test
    names `std/bash` and never pays for a repo.
- **A CURRY NODE IS AN ARGTREE, and an ArgTree is what runs.** SPEC is explicit
  ("we generally talk about ArgTrees, not images; an image is just one arg"), so
  `run-then --run:hash=<curried ArgTree>` is how a staged worker calls one: the
  server's `run_image` unwraps the curry, merges its bound args, and adds `salt`
  and `secret-hash` from the enclosing job. Passing arguments to it IS currying
  them onto it.
  - **`run-request-then` is NOT that verb.** It runs an ArgTree by IDENTITY, for
    a caller that already knows the exact request hash and must run that one
    unmodified — which is what `prepare-request` is paired with. Handed a curry
    node it unwraps nothing, so `args` and `.caos-curry` arrive as two arguments
    of those names and the callee dies on a missing `/cas/args/<its own arg>`.
    That failure reads like "a curry node is not a request", which it is not:
    it is the wrong verb.
- **`salt`, `base`, `secret-hash` and `workerN` are RESERVED entry names.**
  Binding one silently loses: `run_image` merges the run's own salt LAST, so a
  per-test value bound as `--salt` is overwritten at dispatch and never reaches
  the key — two different values then produce one request and nothing re-runs.
  `tests/hello` asserts the list; the suite's own is `--test-salt`.
- **`/cas/args/base` is the IMAGE, not this job's ArgTree.** A later stage must
  re-bind by name whatever it reads. `--test-salt` has to ride in EVERY stage for
  that reason: bound only at the top, a fresh `--test-salt` re-runs the first
  container and hits the memo for all the rest.

- **Everything cargo compiles lives under `rust/`** — `Cargo.toml`,
  `Cargo.lock`, `rust-toolchain.toml`, `crates/`. Run cargo from there, not
  from the repo root. That layout is not tidiness: it is what lets a package
  DECLARE the workspace (`../../rust rust` in a `DEPS`) instead of being handed
  the repository, which is how `tests/cargo-self` and `tests/unit-*` became
  self-contained. It also retired an exclusion in the flake's `src` filter —
  rooted at the repo, `cleanCargoSource` swept the suite's cargo FIXTURES
  (`tests/cargo-check/{broken,mini}`, `tests/cargo-crates/ws`) into the
  dependency key, so editing one rebuilt ~176 deps.
