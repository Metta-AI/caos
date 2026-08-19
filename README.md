# caos

Caos is a Content-Addressable Operating System. It's functional programming with git as the values and docker as the
functions, cached by redis

# Why?

## Security

Every package in your supply chain, and your agent, runs with full access to your computer (by default) and the auth tokens to that allow you to interact with github and many other services

Caos runs all of these pieces in separate containers, with just the permissions that they need

## Performance

Today, when you or an agent make a change, you clone the repo or make a new worktree, then edit a file. You build and test everything. Unless you use bazel, it doesn't matter that the CI server has built and tested most of this already. You do it again, unless (maybe) you have a cache from a different worktree. When you push your code, the CI server checks it all out and builds and tests it all again. Or it copies a large cache file, that often isn't quite the right version, and still builds and tests more than you needed

Caos breaks building and testing into small pieces and caches the results. When you build and test, we never materialize the whole tree

## Location independence

Today, most people run most of their agent workloads on their local machine for convenience. When the work no longer fits, they buy a desktop and try to interact with it over tmux. If the work grows further, they have to split it up between cloud instances. If an agent wants to spin up subagents on other computers, it gets even more annoying

Caos runs well-defined binaries with well-defined inputs and well-defined environments. The work can move seamlessly between computers

# What

* Your code is already in git. You already know docker
* Caos provides the glue to use git as a distributed file system and docker containers as functions. We cache the results in redis
* Workers (containers) receive their inputs as git objects, and lazily load only as much as they need. They stage their results into git. None of this is committed or clogs your main git repo
* Workers can call other workers. They can also define other workers in their git return values (similar to functional programming)
* Once we've run a worker with an input, we cache the mapping to the output value and reuse it for future requests


| Crate | Binaries / image | What it is |
|---|---|---|
| `caos` | `caos`, `caos-cli` | One library, two clients. `caos` is the worker-side client (baked setuid into worker images at `/bin/caos`); `caos-cli` is the user-facing client. See [clients](#the-two-clients). |
| `caos-world` | — | The build's world tag, shared by the three crates that speak the server protocol (`caos`, `server`, `runnerd`) so they cannot disagree about their world. |
| `server` | `caos-server` | One daemon: object storage, compute, and a git smart-HTTP transport, over its own repo. See [server](#server). |
| `runnerd` | `caos-runnerd` | The generic host agent: long-polls the server for jobs and runs worker containers. The server itself runs nothing. See `design/runner-protocol.md`. |
| `worker-common` | — | Shared library for the Rust workers. |
| `worker-runner` | — (`std/runner`'s `/worker`) | The in-image runner trampoline: receives a compiled worker binary as its `worker1` arg and execs it, so every compiled worker rides one shared image as `curry(std/runner, worker1=<binary>)`. |
| `worker-rustc` | — (run as `curry(runner, worker1)`) | Builds a worker from Rust source. See [workers](#workers). |
| `worker-bash-tool`, `worker-llm-call`, `worker-llm-step`, `worker-rgrep` | — (run as `curry(runner, worker1)`) | The agent harness: bounded bash, stateless LLM calls, durable LLM turns, and recursive grep. See `design/chat.md` for the current chat protocol and `design/agent-harness.md` for its historical rationale. |
| `worker-cargo` | — (`std/cargo`'s `/worker`) | Whole-workspace `cargo check/build/test` as `std/cargo` (pinned toolchain + pre-compiled deps + this binary as `/worker`; its image is host-built and streamed like the runner, `caos-worker-cargo-docker` — see `std/cargo/` and `design/cargo-workers.md`) — the agent's `build`/`test` tools. |
| `llm-stub` | — | Scripted `POST /v1/messages` stand-in for the LLM worker tests. |

## Prerequisites

- [Nix](https://nixos.org/download) with flakes enabled.
- Docker, to load and run the images.

No Rust toolchain is needed system-wide; the flake pins it.

## Layout

| Path | Purpose |
|---|---|
| `flake.nix` | Dev shell, binary packages, and Docker images — all from one pinned toolchain |
| `rust-toolchain.toml` | Pins the compiler (`stable` + clippy/rustfmt/rust-src) and the static `musl` target |
| `Cargo.toml` | Workspace root (members + shared release profile) |
| `crates/caos/` | The `caos` crate: shared `lib.rs` + `caos` and `caos-cli` binaries |
| `crates/server/` | The `server` crate → `caos-server` |
| `crates/worker-*/` | The worker crates |
| `build-builtins.sh` | Bootstraps the seeded core and publishes `refs/caos/seed` |
| `caos-tools/`, `tests/` | The `build`/`test`/`test-result` tools and the integration suites |

## Development

Enter a shell with the pinned `rustc`, `cargo`, `clippy`, `rustfmt`, plus
`rust-analyzer` and `cargo-watch`:

```bash
nix develop
```

Inside it, use Cargo as normal (`cargo build`, `cargo run`, `cargo test`).
`nix flake check` runs clippy, rustfmt and the doc build.

**Tests run through caos, not through nix**: `caos-cli run-tool test` (see
[local testing](#local-testing)) runs the unit tests and every integration
suite. That is the only place the unit tests can pass — several spawn `git`,
which the cargo worker's PATH carries and a nix builder's does not — so it is
the signal to trust before committing.

> Nix flakes only see files **tracked by git** (uncommitted edits to tracked
> files are included, but new files are not). After adding a new source file,
> `git add` it before building.

## Building

```bash
nix build .#caos              # ./result/bin/{caos,caos-cli}
nix build .#server            # ./result/bin/server
```

Binaries are statically linked against `musl` — no shared-library dependencies.

Docker images (crates are unprefixed; images carry a `caos-` prefix):

```bash
nix build .#caos-worker-flake-builder-docker   # image tarball at ./result

docker load < result
```

**The stack itself is one image** (`design/one-stack-image.md`): redis, the
registry, the server and runnerd run as a process group inside a single
container, started by `caosd serve`. `caosd up` runs that image; the test suite
runs the same image in the `test` world, so the suite exercises the stack the
host actually runs rather than an approximation of it.

Only a few images are nix-built here: the `flake-builder`
(`caos-worker-flake-builder-docker`), the shared `runner`
(`caos-worker-runner-docker`) and the `cargo` worker
(`caos-worker-cargo-docker`) — each host-built and streamed. Every other std
worker is a flake-built worker image (`std/bash`, `std/merge` — complete flakes
in `std/`, imaged on demand by the flake-builder) or a
`curry(std/runner, worker1=<binary>)` over the runner —
see `design/flake-images.md`.

> On macOS, see [Building on macOS](BUILDING_ON_MACOS.md).

## The big picture

- A **server** holds the canonical CAS and runs compute. It exposes three faces
  over one URL: an HTTP object API (`/object`), an HTTP compute trigger
  (`/run`), and a **git smart-HTTP transport** over its own repo.
- A **worker** is a container run by a **runner** (`caos-runnerd`, the generic
  host agent — the server itself runs nothing; see
  `design/runner-protocol.md`). It reaches the server over HTTP, reading inputs
  from and writing results to a per-job `/cas` directory through the setuid
  `caos` binary, and may stay warm to take further jobs for its image.
- A **user** drives it all with `caos-cli` from inside a git working tree that
  has the server configured as a remote named `caos`. Objects are built locally
  and exchanged with the server by **negotiated git push/fetch**, so passing a
  large, mostly-unchanged tree only transfers the delta.

Everything — an input file, a worker image, a result — is a git object named by
its hash, so identical work is deduplicated and memoized.

## server

One daemon (`crates/server`), image `caos-server`, serving everything over a
single URL. It backs onto a git repository it **owns** (mounted at `/git`); in
dev, `caosd up` creates a dedicated bare repo for it under `CAOS_DATA` (see
[local testing](#local-testing)).

It serves requests **concurrently — one thread per request** — so a worker can
fetch objects while its own `/run` is in flight, and several top-level runs can
proceed at once. Ordinary dependent sub-computations use a **map-then
continuation**: the worker records the continuation as its result and finishes
its job before the server resolves it (see [compute](#compute)). `run-async` is
the deliberate exception: it starts a detached `/run` request from a worker and
returns the request hash immediately, without waiting for that subrequest.
Capacity lives runner-side: the set of hanging `/runner/poll`s *is* the pool.

| Request | Behaviour |
|---|---|
| `GET /object/<hash>` | Return the serialized object (`<type> <size>\0<content>`, the bytes git hashes). `400` if malformed, `404` if absent. |
| `POST /object/` | Store the serialized object in the body, return its git hash. Content-addressed, so idempotent. |
| `GET /run?req=<argTreeHash>&trace=<traceId>` | Run the ArgTree `<argTreeHash>` (`req` is the query param's historical name; its value is the ArgTree hash) and return `"<type> <hash>"` (the fully-resolved result), optionally emitting trace events. See [compute](#compute). |
| `GET /trace/<traceId>/stream` | Stream one live trace as Chrome `B`/`E` events in JSONL. |
| `POST /runner/poll` | A runner's hanging request for work, carrying its required args (name → oid). Answered with a job, `idle` (TTL expired), or `exit` (eviction). See `design/runner-protocol.md`. |
| `POST /runner/result` | A runner posting a job's outcome, keyed by (req, nonce) — first post per nonce wins. |
| `GET /info/refs?service=…`, `POST /git-upload-pack`, `POST /git-receive-pack` | Git smart-HTTP, delegated to `git http-backend` — this is the `caos` remote clients push to and fetch from. |

The git transport is what makes the server a `caos` remote: `git http-backend`
runs `upload-pack`/`receive-pack` over the same `/git` repo. CAOS installs no
ref-policy hooks: clients own any naming, ancestry, and update discipline above
ordinary Git's atomic ref operations. The dedicated repo is created with
`http.receivepack=true` (to accept pushes) and
`uploadpack.allowAnySHA1InWant=true` (so a client can `git fetch` a result by
its bare hash; `/object` itself never needs that flag).

Environment overrides: `SERVER_ADDR` (`0.0.0.0:80`), `CAOS_GIT_DIR` (`/git`),
`CAOS_REGISTRY_PUSH_URL` (`http://caos-registry:5000`),
`CAOS_REGISTRY_PULL_HOST` (`localhost:5000`), `CAOS_REDIS_ADDR`
(`caos-redis:6379`), `CAOS_RUNNER_TOKEN` (unset = runner auth disabled).
Worker-running knobs (network, docker binary, slots) live on `caos-runnerd`.

### Compute

A run **request** is a **WorkRequest**: an **ArgTree** to run, plus runtime
context (an ancestor stack for cycle detection and an optional trace id) that is
NOT part of the cache key. The ArgTree is itself a content-addressed git object,
whose hash, `argTreeHash`, *is* the cache key and the rendezvous id — with
nothing keyed alongside it. The worker image rides *inside* the ArgTree, under a
reserved `base` entry — as do the standard library `std` (a reserved `std`
entry naming the std tree) and the cache-busting `salt` (a reserved `salt`
entry) — so a computation is identified entirely by its args (an executor can
match on the worker alongside the rest, and a worker, seeing its args at
`/cas/args`, can read its own image to call itself). `GET /run?req=<argTreeHash>`
(`req` is the query param's historical name; its value is the ArgTree hash):

1. **read and validate** the ArgTree, whose `base` entry is the worker ref,
   `std` names the standard library, and `salt` is the cache-buster. Direct
   Docker refs and Docker bases inside git images must be digest-pinned; this
   check happens even when a cached result exists;
2. **cache** lookup in Redis keyed on `argTreeHash` — a hit returns the cached
   `"<type> <hash>"` and skips everything below;
3. **cycle check** — the server threads the chain of in-progress `argTreeHash`es
   through its promise sub-runs (below); re-entering one on the stack has no
   fixpoint, so the run fails listing the cycle;
4. **join an in-flight request** — the first cold miss for an `argTreeHash` is
   the owner; concurrent arrivals wait for its exact outcome. An arrival runs
   independently only when waiting would close a cross-thread dependency cycle,
   allowing the ordinary stack check to report that cycle instead of deadlocking;
5. **resolve the image** — a digest-pinned `docker://<name>@sha256:<digest>` is
   used directly; one of our git images is converted to a real image, pushed to
   the registry, and run by digest (see [git images](#git-images));
6. **dispatch to a runner** — the job is matched against the hanging
   `/runner/poll`s (a runner's required args are name → oid pairs the
   ArgTree's top level must equal; most specific match wins, so a warm runner
   already running this image beats the generic `caos-runnerd`, which starts a
   fresh container `/bin/caos runner --job=<json>`);
7. the runner posts back either the result, `"<type> <hash>"`, or a
   **promise**, `"promise <hash>"`: a map-then continuation the worker recorded
   instead of a value (see
   [map-then](#map-then-sub-computations-without-blocking)). The worker has
   already moved on; the server **resolves** the promise — running `map` over
   the children in parallel, then `then` — through this same pipeline, so
   sub-runs are cached, cycle-checked, and may themselves promise;
8. **cache** the resolved result, and for an **external** run (one that arrived
   over HTTP) pin `refs/caos/res/<argTreeHash>` at it as a durability and Git
   negotiation anchor. Result refs are hidden from upload-pack/fetch
   advertisements but remain visible to receive-pack negotiation. They are not
   a result-query API; callers already receive the result hash. Sub-runs set no
   ref.

Results stay on the server. The caller gets back the hash and a type; it does
**not** receive the bytes unless it asks (see [result handling](#requests-and-results)).

### Map-then: sub-computations without blocking

A worker never blocks on another worker. Its `caos map-then` is a **tail call**: it
records a continuation `{in, map?, run?, then?}` — `in` a tree entry for the data
node, `map`/`run`/`then` blobs naming images (`map` and `run` mutually
exclusive) — as the worker's own result at
`/cas/out`, and the worker's job is done. The server then:

1. if `map` is given and `in` is a tree: runs `map --in=<child>` for **each
   child of `in`, in parallel** (a blob `in` is a leaf — no children), and
   assembles the results into a `children` tree under the original names; if
   `run` is given (`caos run-then`, the single-valued form): runs
   `run(--in=<in>)` once, yielding R;
2. produces the request's result: `then(--in=<in>[, --children=<children> |
   --result=<R>])` if `then` is given (the extra arg only when a `map`/`run`
   ran), else the `children` tree / R itself. With neither, `then(--in)` is a
   plain tail call.

Recursion ties the knot through `map`: a worker curries *its own image* — read
straight from `/cas/args/base`, the request's reserved entry — as the mapper,
so each child gets the same treatment, with no std lookup and for any git
image (a rustc-built worker as much as a builtin) — and each child may itself
promise. Because a worker either computes a value or *describes* the remaining
work, only server threads ever wait; a bounded runner pool always drains. See
`design/map-then.md` for the full argument.

### Caching

Results, converted images, and built layers are cached in Redis
(`caos:result:<argTreeHash>`, `caos:image:<git-hash>`, `caos:layer:<tree-hash>`).
A hit on the result key skips the container entirely (logged `cache hit …` vs
`cache miss …`). Redis is best-effort: if it's unreachable the server logs and
runs uncached. Cold misses are single-flighted in the server: identical
requests join one live owner, and image conversion and layer publication use
the same per-key pattern. A request runs independently only when the waits-for
graph proves that joining would form a cycle. Losing an owner fails and wakes
its waiters so a later arrival can become the new owner.

### Git images

A non-`docker://` image is the git hash of an image in **git-docker form** — a
tree of `config.json` plus one `layer<NN>` subtree per layer (the layer's
extracted filesystem). The server converts it to a real image:

- each `layer<NN>` tree is materialized and tarred (uncompressed, GNU format,
  zeroed owners/mtimes, sorted) — `digest = sha256(tar)`;
- `config.json`'s `rootfs.diff_ids` are **generated** from those layer hashes
  (uncompressed ⇒ a layer's digest *is* its diff_id), so the producer needn't
  supply diff_ids and per-entry perms/ownership ride in `.caosmeta` sidecars;
- an OCI manifest is pushed by digest.

Deterministic, so it's Redis-cached by git hash. The registry is reached two
ways for one instance: the server pushes by name on the docker network
(`CAOS_REGISTRY_PUSH_URL`), the host daemon (which runs the worker) pulls via the
published port (`CAOS_REGISTRY_PULL_HOST`, insecure, no TLS).

## The two clients

`crates/caos` is one library with two binaries. They share all the object
logic — the difference is the **transport** and the privilege model.

- **`caos`** (worker-side) talks to the server over **HTTP** (`/object`), and
  provides the container `runner`. It's installed **setuid-root** in
  worker images so an unprivileged worker can reach the root-owned `/cas` only
  through it. Subcommands: `get-hash`, `get`, `put`, `put-commit`, `hash`,
  `map-then`, `run-then`,
  `curry`, `runner`. Its `map-then`/`run-then` are *tail calls* — they record a
  continuation
  as the worker's result (see [map-then](#map-then-sub-computations-without-blocking));
  it never triggers compute itself.
- **`caos-cli`** (user-facing; also installed as plain `caos`) uses the server
  as a **`caos` git remote**: it builds objects in the local working repo and
  exchanges them by negotiated push/fetch. It has no `/cas` and no
  object-level commands:
  - `run` — compute (blocking, as before), with the result checked out to any
    host path;
  - `curry` — bind args to an image, printing the curried ref;
  - `import-image` — get a docker image into caos, printing its hash;
  - `talk` / `chat` — agent conversations over the current protocol
    (`design/chat.md`; `design/agent-harness.md` records historical rationale);
    `caos talk "<prompt>"` is the everyday form;
  - `secrets [--check]` — tend the git-ignored `.caos-secrets` store: fill a
    missing `entropy=`, warn on a weak one (`--check` reports only and exits
    non-zero, for CI). Offline — no server (design/secrets.md).

`caos-cli` must run inside a git working tree with the server as its `caos`
remote — the remote's URL is also where compute is triggered and results are
fetched, so there is nothing else to configure:

```bash
git remote add caos http://localhost:9090
```

### The CAS and `/cas`

`/cas` is a **worker** thing — there's no CAS on the host. Inside a worker the
`caos` binary materializes objects under `/cas`, and every materialized path is
tagged with the git hash it came from in the `user.caos.hash` xattr — the
on-disk, per-path mapping from a path back to its hash. Writes are atomic (build
in a temp sibling, set the xattr, `rename` into place), so concurrent runs never
see a half-written path; startup probes that the filesystem supports `user.*`
xattrs.

A `/cas` path is **single-assignment**: `get-hash`/`put`/`map-then` refuse a
path that already exists, so a recorded result — in particular the promise
placeholder `map-then` seals at `/cas/out` — can never be silently replaced.

`get-hash <hash> <path>` materializes an object at `<path>` (a direct child of
the CAS): a **blob** becomes a file; a **tree** becomes a directory of one-level
**placeholders** (empty, hash-tagged — a dir for subtrees, a file otherwise).
`get [-r|--recursive[=<n>]] <path>` expands a placeholder in place: one level by
default, `<n>` levels, or the whole subtree with `-r`. So a worker drills down a
tree lazily, one level at a time, and `get -r` is idempotent/resumable.

`/cas` is genuinely protected (see [permissions](#permissions-load-before-read-and-no-tampering)):
everything is root-owned, and the unprivileged worker reaches it only through the
setuid `caos`.

### Requests and results

`caos-cli run [--trace[=<file|->]] [--trace-id=<id>] [output] --base:<type>=<image> [--name=value | --name:@=path …]` (the
blocking, user-facing run):

1. assembles the args into a git **tree** — the **ArgTree** — including the
   `--base` image under the reserved `base` entry and (when set) the cache-busting
   salt under a reserved `salt` entry (see
   [arguments](#arguments-literals-paths-and-pinned-refs));
2. the ArgTree's hash *is* the content-addressed request id (`argTreeHash`) —
   nothing wraps it, so the ArgTree is the whole cache key;
3. gets the ArgTree onto the server — one negotiated `git push` to
   `refs/caos/req/<argTreeHash>`, whose reachable graph includes any embedded
   git-image tree, so the image needs no separate push;
4. calls `/run?req=<argTreeHash>`; the server resolves any promises before
   answering, so the reply is always a final value;
5. records the result at `<output>`: it **checks the result out in full** —
   fetching the object and (for a tree) every descendant as ordinary rw files
   (`0644`/`0755`, git's executable bit preserved), so it's readable and
   editable on the host directly. `<output>` is optional: with it omitted, a
   **file** result is streamed to **stdout** (handy for `| less` or `> file`);
   a **tree** result has no single stream, so it still needs an `<output>` path.

Pass `--trace=<file>` to write Chrome Trace Events as JSONL. `--trace` and
`--trace=-` write to stdout and require a separate computation output path.
`--trace-id=<id>` optionally overrides the generated invocation id.

```sh
caos-cli run --trace=trace.jsonl <result-path> --base:<type>=<image> --input=value
caos-cli run --trace <result-path> --base:<type>=<image> --input=value
```

Traces are live-only and discarded when the run ends. Trace ids do not affect
request or cache identity.

The worker-side `caos map-then <in> [--map:<type>=<image>] [--then:<type>=<image>]` is a
different thing entirely: a **tail call**. It records the continuation
`{in, map?, then?}` as the worker's own result at `/cas/out` (a `promise`
placeholder) and fetches and runs nothing; the worker exits and the server
takes over (see [map-then](#map-then-sub-computations-without-blocking)). So a
worker's sub-results never come back to it at all — child results are wired
into `then`'s `--children` tree by the server, by hash, and only at the top,
where `caos-cli` returns the final result to the user, is the whole tree pulled
down.

**Failures propagate.** If a worker exits non-zero, the runner posts a failure
result carrying the worker's log, and the server answers `/run` with that
error — and a failure anywhere in a promise tree (a `map` child, a `then`,
any depth) fails the requests above it the same way, up to the top-level
`caos-cli run`, which exits non-zero with the message. (The run-cycle error is
one such case.)

**An image is named by an operator, never by shape.** `--base` (and a
`--map`/`--run`/`--then` value) takes the same type tags as any other argument —
there is no positional image anywhere, and nothing sniffs a bare token:

- `:hash=<oid>` — an object the server holds: a git image, or a `caos curry` ref
  or `import-image` output;
- `:@=<path>` — on `caos-cli` a host DIRECTORY, which is INGESTED and then
  EVALUATED (a tree carrying a `.caos-expr` resolves to what that expression
  builds, one without it to itself); inside a worker any `/cas` path, resolved to
  the hash recorded on it;
- `:docker=<ref>` — a **digest-pinned docker image** (`<name>@sha256:<digest>`),
  stored as the blob `docker://<ref>`;
- `:@@=<git ref>` — a worker that lives in **another repo**, pinned by commit
  sha and fetched by the client (see the arg types below). This is how a project
  depends on caos without vendoring it.

### Arguments: literals, paths and pinned refs

An argument's type is chosen by the *operator* — never by sniffing the value —
so a value is never misread and may contain anything (no escaping):

- `--name=value` → a literal string, stored as a blob;
- `--name:@=path` → a path (the `@` nods to curl/HTTPie). It's resolved doing as
  little work as possible:
  - a `/cas` path (worker) → reference the hash recorded on it (no read);
  - a host path (caos-cli) → ingest via git, reusing git's own objects. Only
    **git-tracked** paths are visible — like a nix flake, a build sees only what
    git knows about, so an untracked file is never shipped:
    - **clean + tracked** → reuse the committed hash from `git ls-tree HEAD` — no
      read at all, so a large unchanged directory is effectively free;
    - **tracked file, uncommitted edits** → `git hash-object -w` on the working
      tree bytes;
    - **tracked directory, uncommitted edits** → copy `.git/index` to a throwaway
      index and `git add -u` + `write-tree --prefix` there, so only the
      **changed** tracked files are re-read (the stat-cache covers the rest),
      untracked files are excluded, and your real index is untouched — the trick
      `git stash`/`commit` use;
    - **untracked, or outside the worktree** → an error;
  - a **missing** path is an error, not silently a literal.
- `--name:commit=value` → a **commit**, passed *unpeeled* as a gitlink entry
  (the default forms peel commits to trees, which image refs depend on). The
  value is a bare commit hash, a `/cas` path recorded as a commit (worker), or
  a revspec like `HEAD` resolved in the working repo (caos-cli). Inside a
  worker a commit is a file holding the raw commit object; `caos put-commit`
  mints one (at `/cas/out` it makes `commit <hash>` the run's result). See
  `design/commits.md`.
- `--name:hash=<oid>` → an object the server already holds — a tree or a blob,
  typically an earlier run's result — referenced by oid with no round-trip;
- `--name:docker=<ref>` → the blob `docker://<ref>` (when used as an image,
  `<ref>` must contain `@sha256:<digest>`);
- `--name:@@=<git ref>` → a tree in **another repo**, named by a nix-style
  flake-reference (`git+https://host/repo?rev=<40-hex>&dir=sub`, `git+ssh://…`,
  `git+file://…`, `github:owner/repo`, or a local `path:./dir`). **A URL is a
  name; a hash is content**, so `rev` is mandatory and must be a full commit
  sha — a `ref=` naming a branch is refused, because a mutable input has no
  business in a cache key. The **client** resolves url+rev → oid at eval time
  (fetching the closure), and the arg entry is that oid, byte-for-byte what a
  local `:@=` of the same content would produce: the URL never enters an
  ArgTree, so two consumers pinning the same rev share the whole subgraph by
  hash. Resolving a locator is a **client** step — not because a worker lacks a
  network (it has one), but because the ArgTree is the cache key: the locator
  has to become an oid before the request exists, or a name would sit inside
  content-addressing. See `design/flake-inputs.md`.

The grammar is `--name[:type]=value` and extensible: a new type is a variant, a
parse arm and a case in each resolver. The worker `caos` has no host filesystem
(only `/cas`), so a non-`/cas` path there is an error.

### Other subcommands

`curry` and `import-image` are the other `caos-cli` commands; the rest are
**worker** (`caos`) commands, operating on `/cas`.

- `import-image <docker-archive>` (`caos-cli`) — store a docker-archive image
  (`nix build .#caos-*-docker` output) as a git-docker tree on the server,
  printing its hash. Used to ingest images into caos so they can be `run` (and to
  assemble the std library — see `build-builtins.sh`).
- `put <src-path> <cas-path>` (`caos`) — store an outside path into the CAS and
  record it at a `/cas` path. Files become blobs, directories trees; a symlink
  into the CAS reuses the recorded hash.
- `curry [--unbind=<name> …] --base:<type>=<image> [--name=value | --name:@=path …]`
  (both clients) — bind some
  args to an image, printing a ref to the curried image. It's a small
  content-addressed tree (`base`, `args`, a `.caos-curry` marker); `run`/`curry`
  expand it — the CLI for its own calls, the server when a curried `map`/`then`
  runs (call args win, and the base is folded into the args tree as its
  `base` entry) — so a request only ever carries a plain args tree. Currying
  flattens, so it's canonical. On `caos-cli`, path args are host paths to
  ingest.
- `runner --job=<json>` (`caos`) — the container runner; see below.

### `runner`

`caos runner --job=<json>` runs jobs inside the container until an idle TTL
passes (see `design/runner-protocol.md`). Per job:

1. **unpack** — fetch the request tree named by the job's `req` (it IS the
   ArgTree) and read its reserved `salt` entry (image and salt both ride inside
   it);
2. **set up** — wipe and recreate `/cas`, root-owned, and verify xattrs;
   materialize the args at `/cas/args`;
3. **run `/worker`** — dropped to the unprivileged `worker` user so it can't
   touch the root-owned `/cas` except through setuid `caos`; the runner stays
   root to tear down. The worker's output is relayed to the container log and
   rides along with a failure report;
4. **report** — POST `"<type> <hash>"` for `/cas/out` (a fast xattr read plus
   an `is_dir` check — no re-hashing) to `/runner/result`. `blob`/`tree`
   results go back to the caller as-is; a `promise` (a `caos map-then`
   continuation) is resolved by the server once posted;
5. **tear down** — delete `/cas`, then long-poll `/runner/poll` for another job
   for this image (`required: {image: <oid>}`). An `idle` or `exit` reply ends
   the container; a job goes back to step 1 — that's the warm-worker win: no
   container start between jobs.

So a `/worker` reads inputs from `/cas/args` and writes its result to
`/cas/out`.

### Permissions: load-before-read, and no tampering

In a worker, `/cas` is locked down (everything root-owned), two rules enforced by
file modes:

- **Nothing is readable until fetched.** Placeholders are owner-only
  (`r--------`/`r-x------`); `get` makes loaded content world-readable. So a
  worker reads only what it explicitly loaded.
- **The worker can't tamper with `/cas`.** It runs unprivileged and mutates
  `/cas` only through `caos`, which is **setuid-root** in the image (and static,
  so no dynamic-linker attack surface).

There's no `/cas` outside a container: `caos-cli` never materializes objects
locally — it pushes/fetches git objects and checks a `run` result out as ordinary
files.

## Workers

A worker image is an image whose definition includes `/worker` — the
executable `caos runner` execs. An *interpreter* image's `/worker` runs its
argument (the `worker1` arg — like `python:3` runs a script), which is how
the compiled workers below ride one shared image: each is
`curry(std/runner, worker1=<binary>)`, and the binary reads `/cas/args` and
writes `/cas/out`. The Rust workers share `worker-common` (arg helpers,
`caos`/`map_then`/`caos curry` wrappers, result staging).

The example workers are TEST FIXTURES, not std entries: each test carries
its worker's source (`tests/<name>/worker.rs`, single-file, linking only
`worker-common`) and builds it with `std/rustc` at test start — memoized,
one compile per source edit. `hello.rs` in `examples/consumer/` shows the
same flow for consumers. The fixtures worth reading:

- **file-count** (`tests/file-count/worker.rs`) — counts the leaf files
  under `--in`, recursing with itself through map-then: one image, three
  positions — the shape any structural fold takes here. Identical subtrees
  are memoized; siblings count in parallel.
- **dirs-only** (`tests/dirs-only/worker.rs`) — keeps only a node's
  directory children, dropping files.
- **deep-deps** (`tests/deep-deps/worker.rs`) — transitive dependencies by
  self-recursion; recompute is O(changed package + its dependents).
- **`worker-rustc`** — builds a runnable worker from a Rust source file, as
  pure orchestration over the cargo worker: it lays out a project (the source
  plus the curried-in `worker-common` tree), tail-calls the cargo worker to
  compile it static-musl, and curries the binary into the runner —
  `curry(runner, worker1=<binary>)`. So building a worker is itself a (memoized)
  worker, and no toolchain image is dedicated to it.

### The standard library (`std/`)

There is no ambient standard library: no `/cas/std`, no `refs/caos/std`, and no
`std` entry in a request. Every `std/<name>` is a checked-in source directory
whose `.caos-expr` says how it is built, and a caller reaches one by DESCENT — a
`DEPS` line naming a path, expanded by a root `.caos-expr` into a
`DEEP-DEPS/<name>` mount (`design/caos-expr.md`).

A workspace declares what it reaches for in its own `DEPS`:

```
./std/bash bash
./std/llm-step llm-step
```

and a repo that mounted caos writes the same lines against
`./flake-inputs/caos/std/...`. Relative paths are stable under mounting, so the
declaration moves and the code does not.

The five entries that cannot be built by the machinery they ARE — `flake-builder`
(a flake built by the flake-builder), `cargo`, `runner`, `rustc` and `deep-deps`
— name a `docker://seeded…` sentinel instead of a builder. `./build-builtins.sh`
hand-builds what each expression would have produced and publishes it as a seed
record under `refs/caos/seed`; the core-seeder-runner answers that exact key,
spawning no container.

```bash
./build-builtins.sh                 # bootstrap the seeded core
./build-builtins.sh bash cargo      # a subset
```

## Local testing

- Build the stack with `nix build`
- Run the dev stack with `result/bin/caosd up`
- **Check `caosd version` before believing a bug report.** A devShell that fails
  to build leaves direnv on the *previous* environment, so the `caosd` on PATH
  can be far older than the `flake.lock` that names it — and the symptom is an
  error that reads like a caos bug. `caos-cli`'s usage banner carries the same
  revision.
- Test with `caos-cli run-tool test`. This builds and tests. Each test gets a stack, built from source. No need to rebuild or restart caosd

```bash
caosd up      # bring the stack up + publish all of std, then return. Updates it if already running
caosd logs    # follow the running stack's logs (Ctrl-C returns; stack stays up)
caosd down    # stop it (Redis + registry volumes and the server repo are kept)
caosd reset   # stop and wipe those volumes + the server repo for a clean slate
caosd version # the caos revision this command was built from
```

**Is the installation working?** `std/hello` mirrors its arguments back, which
exercises the whole path — a client forms an ArgTree, the server schedules it, a
runner starts a container, a worker reads `/cas/args`, a result comes back — and
puts the answer on stdout in one command:

```bash
caos-cli run --base:@=DEEP-DEPS/hello --greeting=hi --who=world
# hello: 2 arguments
#   greeting = hi
#   who = world
```

Declare it first (`./std/hello hello` in your `DEPS`, or reach it by locator:
`--base:@@=git+https://github.com/Metta-AI/caos?rev=<sha>&dir=std/hello`).

```bash
caos-cli run-tool build      # the worker images, from the deployed binaries
caos-cli run-tool test       # images + the whole test suite
caos-cli run-tool test --only="unit-test rgrep" # just these tests (cache shared
                                    # with full runs, both directions)
CAOS_SALT=$(date +%s) caos-cli run-tool test   # force a re-run (retry a flake)
```

nix builds only the *host* stack — the server, the runner daemon, the seeder,
and the seeded core images. Everything the suite tests is compiled from the
tree under test, inside caos, by `std/cargo` and `std/rustc`; no host binary
is handed in. (After a Rust edit: `nix build && caosd up`, then test. After
editing anything under `std/`, the same — a std tree is part of the seed keys.)

The test tool (`caos-tools/test.sh`, carried by this tree) is the suite
worker, in five stages of one script: `suite` builds the worker images via
`caos-tools/build.sh`, `deepener` and `deepen` expand every test's `DEPS`
into `DEEP-DEPS/` mounts, `fanout` runs one job per `tests/<name>/cli.sh`,
and `summarize` assembles the report. A test is a directory `tests/<name>/`
with a `cli.sh`, which runs inside a test-stack worker, cwd'd into a client
repo with the test tree at `./test` and `$CAOS_CLI` set, driving
computations through `caos-cli` against a nested caos stack built from
your edited tree. New tests are picked up automatically. Results — every
test's verdict, full output, and the inner stack's logs — land as a git
tree pinned on the server. `run-tool` materializes none of it: it prints the
result hash, then reads just the report — two objects.

The report is an index, not an archive: a line per test with its time and the
hash of its record, then the last 20 lines of each failing test. Read a record
in full with the second tool, by hash — no checkout, one object at a time:

```bash
caos-cli run-tool test-result --hash=<hash>            # the test's full output
caos-cli run-tool test-result --hash=<hash> --log=server  # an inner-stack log
```

To get the whole tree on disk instead, `caos-cli get <hash> <path>`.

## Notes

- **Toolchain version** is whatever `stable` resolves to against the locked
  `rust-overlay` revision in `flake.lock`. Pin an exact version with `channel =
  "1.96.0"` in `rust-toolchain.toml`.
- **Architecture**: the static target is `x86_64-unknown-linux-musl`. On ARM,
  switch both `rust-toolchain.toml` and `muslTarget` in `flake.nix` to
  `aarch64-unknown-linux-musl`.
- **Native (C) dependencies**: a crate linking C libraries (e.g. `openssl`)
  needs a `musl` cross-toolchain to stay static — see the commented
  `buildInputs`/`nativeBuildInputs` in `flake.nix`.
- **Cleanup (dev)**: transient `refs/caos/req/*` are pruned after ten minutes.
  Durable `refs/caos/res/*` are hidden from upload-pack/fetch advertisements but
  remain visible to receive-pack negotiation; they still accumulate
  (content-addressed, so they dedup). A deployment that does not need indefinite
  result retention should define a policy and run `git gc`.
