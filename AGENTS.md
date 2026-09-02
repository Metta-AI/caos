Read ./SPEC.md

# Shell, under `set -euo pipefail`

Every script here runs with it, and two constructs quietly break under it.

- **`[ cond ] && action` exits the script when the condition is false**, if that
  list is the last command in its scope (a function, a loop body, a `{ }` block,
  the script). Write `if`. This is not hypothetical: it is the single largest
  source of bugs in this tree's shell — `stack/build-builtins.sh` still carries three
  latent instances, one of which makes `./stack/build-builtins.sh <name>` exit 1
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
  `sed`**, and no awk. `sed 's/^/  /'` in `std/caos-test/worker.sh` passed two
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

- **`fetch_and_materialize` is the WORKER's materialization, not the host's.**
  Its name reads like the obvious way to get a result onto disk, and host-side
  it is the wrong one: it writes hash-tagged PLACEHOLDERS for a later `caos get`
  to fill, and nothing on the host fills them. So every file arrives
  ZERO-LENGTH and whatever reads them concludes the result was empty — a grep
  over a correct result tree reported "no matches", which is a silent wrong
  answer rather than an error. The host form is `checkout`, public as
  `cli_get(t, hash, path)`: ordinary rw files, exec bit and symlinks preserved.
  It is what `caos-cli run <output>` uses, which is why running the same job by
  hand shows content and the in-process call does not.

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
  completing. **The test harness no longer creates one** (`dev/cli-test/worker`
  says so, and says to look there first if a push ever dies unable to read an
  object), which is what lets `tests/push-closure` reproduce at all — it used to
  have to delete the alternate itself.

- **An unsalted `run-tool caos-test` does not prove a push works.** `ensure_pushed`
  asks before pushing — a `HEAD /object/<argtree>`, and on a hit it returns
  without running git at all — so re-running with an unchanged tree does not
  push, pack or traverse anything, and any defect in forming or packing the
  request is invisible. (It was invisible before that probe too: the push was a
  no-op update of a ref already at that hash.) Only a NEW ArgTree builds a real
  pack — which is why the primary gate is
  `CAOS_SALT=$(date --iso=s) result/bin/caos-cli run-tool caos-test` (SPEC.md) and
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

# Several dev stacks at once

Several `dev/test-stack` containers run against one host stack, each testing a
different tree, and they share three volumes (`/mounted-nix`, `/caos-dev`,
`/caos-images`) plus the host's redis and registry. The rule that makes that
work is **share what is keyed by CONTENT, and give everything else a name of
the container's own.** Where it was backwards, none of the failures looked like
concurrency.

- **A seed record is keyed by NAME, so a second publisher REPLACES yours.** This
  is why the seed tree is now PASSED BY VALUE — `stack/serve` runs
  `stack/build-builtins.sh`, takes the tree it prints on stdout, and starts the
  seeder with it — rather than written to `refs/caos/seed` and polled for. With
  a ref, two trees differing under `std/` differ in `required`, so the loser's
  keys vanish and its jobs park until the 503 that blames capacity; and two
  BUILDS of caos are worse, because `required` is identical and only `result`
  differs, so the loser's seeder silently answers with the winner's binaries.
  The whole visible trace of that was one line —
  `core-seeder-runner: deep-deps now answers tree X (was tree Y)`. Both
  reproduced with two dev stacks against one host. If you ever reintroduce a
  name between a publisher and an answerer, this is what you are signing up for.
- **One fixed path under a shared volume is one stack's answer for all of
  them.** `/caos-dev/bin` was the symlink `std/caos-test` reads to find the
  client it just built, so the stack that started second repointed it and the
  first drove its own server with the OTHER build's `caos-cli`.
  `/caos-dev/publish-client-repo` was two publishers staging `seed-deep-deps`
  into one repo under one name — reproduced as `ln: failed to create symbolic
  link '/caos-dev/publish-client-repo/layer-additions/usr/bin/env': File
  exists`, which killed a whole bring-up. `/caos-dev/stack.ready` is `rm -f`'d
  before serve starts, so a second stack could delete the first's after serve
  wrote it and the first would wait out its 60s and die "never came up". All of
  these live under `/caos-dev/runs/<container id>` now, reached as `/caos-run`
  from inside the container — which is also where a dead stack's logs are.
- **`git config` is a LOCK, and the server treats it as required.** The settings
  a server asserts on EVERY boot are `git config` writes, so two servers coming
  up on one repo race for `config.lock` and the loser died with `fatal: …
  could not lock config file config: File exists` before it ever listened —
  surfacing a file away as `caosd serve: server never came up`.
  `run_required_git` waits a peer's lock out now (5s) instead of dying on it.
- **What IS safe to share is anything content-addressed**, and most of this is:
  the object database (`refs/caos/req|res/` are keyed by hash, and a test that
  writes a mutable ref uniquifies it — `tests/README.md`), the registry, the
  podman store, and redis under its `CAOS_CACHE_NAMESPACE`. The nix store too,
  with one exception: `dev/test-stack/worker` seeds it with `cp`, which writes
  each file in place rather than temp-and-rename, so a concurrent nix can read a
  store path that is half there. That copy takes a flock in the volume.
- **A concurrency fix is not tested by one stack.** Run two, from two trees that
  really differ, at the same time — `git clone --local` two worktrees, put an
  `eprintln!` marker in `worker-deep-deps` in each, `caos-cli run-tool caos-test
  --only=hello` in both at once — and then read
  `/caos-dev/runs/*/logs/core-seeder-runner.log`: each stack must answer its OWN
  `deep-deps` tree, and each `runs/*/bin` must point at its own
  `caos-test-stack-inputs`. A green suite from one of them proves nothing.
  Do NOT diverge the trees by editing a SEEDED std entry (`std/deep-deps`,
  `std/rustc`, `std/runner`, `std/cargo`, `std/flake-builder`): the HOST's
  seeder has to answer for those while resolving `caos-test`'s own image, so the
  run dies at the host with a 503 naming `docker://seeded-…` before either dev
  stack exists. Diverge `rust/` instead.

# Before committing

- **Never read a gate's exit status through a pipe.** `cmd 2>&1 | tail`
  reports `tail`'s status, so a failed command looks like a pass — that happened,
  and the next step ran against a stack that was not up. Use
  `cmd 2>&1 | tail; echo "EXIT=${PIPESTATUS[0]}"`, or don't pipe.
- **`nix build` only sees GIT-TRACKED files.** A flake's source is the git
  tree, so a NEW file that cargo compiles happily is simply absent from the
  build — `mod cc;` failed with "to create the module `cc`, create file
  ...cc.rs" while that exact file sat in the working tree. The error names the
  file it is looking at, which reads as a typo rather than a missing `git add`.
  This is NOT the `src` filter below (a `.rs` file is kept); dirty EDITS to
  tracked files are picked up fine, which is what makes the exception easy to
  forget. `git add` the file before believing a `nix build` failure about it.

- **`run-tool caos-test` does not cover `nix build`.** The suite compiles the tree
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
    a test that drives `caos-cli` names `--base:@=DEEP-DEPS/cli-test` and DEPs on
    `../../dev/cli-test`, whose `/worker` stages the repo and then runs `worker1`.
    Grep `DEPS` for `cli-test` to see which tests are client tests. A worker test
    names `std/bash` and never pays for a repo.
- **`prepare-request` forms a run; `curry` builds an IMAGE.** Handing a curry
  node to `run-request-then` produces an ArgTree the runner does not unwrap, and
  it dies on `caos get /cas/args/worker1` — its own entrypoint arg, missing,
  because the chain below was never merged. Prefer `run-request-then` over
  `run-then` in a staged test: it runs a complete ArgTree unchanged, so no
  `--in` is invented for a callee that does not want one (worker-cargo and
  std/bash-tool both read `--in` in preference to their real argument, so an
  empty one silently shadows it).
- **`/cas/args/base` is the IMAGE, not this job's ArgTree.** A later stage must
  re-bind by name whatever it reads. `--salt` has to ride in EVERY stage for
  that reason: bound only at the top, a fresh `--test-salt` re-runs the first
  container and hits the memo for all the rest.
