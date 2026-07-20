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

Caos runs work well-defined binaries with well-defined inputs and well-defined environments. The work can move seamlessly between computers

# What

* Your code is already in git. You already know docker
* Caos provides to glue to use git as a distributed file system and docker containers as functions. We cache the results in redis
* Workers (containers) receive their inputs as git objects, and lazily load only as much as they need. They stage their results into git. None of this is committed or clogs your main git repo
* Workers can call other workers. They can also define other workers in their git return values (similar to functional programming)
* Once we've run a worker with an input, we cache the mapping to the output value and reuse it for future requests


| Crate | Binaries / image | What it is |
|---|---|---|
| `caos` | `caos`, `caos-cli` | One library, two clients. `caos` is the worker-side client (baked setuid into worker images at `/bin/caos`); `caos-cli` is the user-facing client. See [clients](#the-two-clients). |
| `server` | `caos-server` | One daemon: object storage, compute, and a git smart-HTTP transport, over its own repo. See [server](#server). |
| `worker-common` | — | Shared library for the Rust workers. |
| `worker-hello`, `worker-file-count`, `worker-dirs-only`, `worker-deep-deps`, `worker-rustc` | `caos-worker-<name>` | Example/built-in workers. See [workers](#workers). |
| `worker-bash-tool`, `worker-llm-step` | — (run as `curry(runner, bin)`) | The agent harness: the bounded bash tool and the LLM step driver. See `design/agent-harness.md`. |
| `llm-stub` | — | Scripted `POST /v1/messages` stand-in for the llm-step tests. |

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
| `Tiltfile`, `build-builtins.sh`, `test-*.sh` | Local dev + integration tests |

## Development

Enter a shell with the pinned `rustc`, `cargo`, `clippy`, `rustfmt`, plus
`rust-analyzer`, `cargo-watch`, and `tilt`:

```bash
nix develop
```

Inside it, use Cargo as normal (`cargo build`, `cargo run`, `cargo test`). Run
lint/format/test the way CI does with `nix flake check`.

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
nix build .#caos-server-docker            # image tarball at ./result
nix build .#caos-worker-base-docker
nix build .#caos-worker-hello-docker      # ...-file-count, -deep-deps, -rustc, -bash

docker load < result
```

Or build and load into the local docker daemon in one step (streamed, nothing
large written to the Nix store):

```bash
nix run .#load-caos-server
nix run .#load-caos-worker-hello          # load-caos-worker-{base,file-count,...}
```

Worker images contain **only** their static `/worker` binary plus a setuid-root
`/bin/caos`, the `worker` user (uid 1000), and a writable `/tmp` — no shell, no
libc, no `/nix/store`. The `caos-server` image is not minimal: it bundles the
`docker` client, `git`, and `tar`, and expects the host's docker socket.

> Docker images are Linux-only. On macOS, build the `*-docker` outputs via a
> remote/linux builder; the binaries and dev shell build natively.

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
dev, Tilt creates a dedicated bare repo for it (see [local testing](#local-testing)).

It serves requests **concurrently — one thread per request** — so a worker can
fetch objects while its own `/run` is in flight, and several top-level runs can
proceed at once. Workers never call back into `/run`: a worker that needs
sub-computations records a **map-then continuation** as its result and finishes
its job, and the server resolves it (see [compute](#compute)) — so no worker
ever waits on another worker and nothing can deadlock. Capacity lives
runner-side: the set of hanging `/runner/poll`s *is* the pool.

| Request | Behaviour |
|---|---|
| `GET /object/<hash>` | Return the serialized object (`<type> <size>\0<content>`, the bytes git hashes). `400` if malformed, `404` if absent. |
| `POST /object/` | Store the serialized object in the body, return its git hash. Content-addressed, so idempotent. |
| `GET /run?req=<reqHash>&trace=<traceId>` | Run the request object `<reqHash>` and return `"<type> <hash>"` (the fully-resolved result). The optional trace id emits this invocation to its already-open live stream without changing the request or cache key. See [compute](#compute). |
| `GET /trace/<traceId>/stream` | Follow one live trace as JSONL. Each line is a Chrome Trace Event, and the server removes the trace when the run ends. |
| `POST /runner/poll` | A runner's hanging request for work, carrying its required args (name → oid). Answered with a job, `idle` (TTL expired), or `exit` (eviction). See `design/runner-protocol.md`. |
| `POST /runner/result` | A runner posting a job's outcome, keyed by (req, nonce) — first post per nonce wins. |
| `GET /info/refs?service=…`, `POST /git-upload-pack`, `POST /git-receive-pack` | Git smart-HTTP, delegated to `git http-backend` — this is the `caos` remote clients push to and fetch from. |

The git transport is what makes the server a `caos` remote: `git http-backend`
runs `upload-pack`/`receive-pack` over the same `/git` repo, with hooks intact
(so a `post-receive` trigger is a natural future evolution). The dedicated repo
is created with `http.receivepack=true` (to accept pushes) and
`uploadpack.allowAnySHA1InWant=true` (so a client can `git fetch` a result by
its bare hash; `/object` itself never needs that flag).

Environment overrides: `SERVER_ADDR` (`0.0.0.0:80`), `CAOS_GIT_DIR` (`/git`),
`CAOS_REGISTRY_PUSH_URL` (`http://caos-registry:5000`),
`CAOS_REGISTRY_PULL_HOST` (`localhost:5000`), `CAOS_REDIS_ADDR`
(`caos-redis:6379`), `CAOS_RUNNER_TOKEN` (unset = runner auth disabled).
Worker-running knobs (network, docker binary, slots) live on `caos-runnerd`.

### Compute

A run **request** is itself a content-addressed git object: a tree
`{args, std, salt}` whose hash, `reqHash`, *is* the cache key and the
rendezvous id. The worker image rides *inside* `args`, under a reserved `image`
entry — so a computation is identified entirely by its args (an executor can
match on the worker alongside the rest, and a worker, seeing its args at
`/cas/args`, can read its own image to call itself). `GET /run?req=<reqHash>`:

1. **read** the request tree (`args` tree — whose `image` entry is the worker
   ref — plus the `std` tree and `salt`);
2. **cache** lookup in Redis keyed on `reqHash` — a hit returns the cached
   `"<type> <hash>"` and skips everything below;
3. **cycle check** — the server threads the chain of in-progress `reqHash`es
   through its promise sub-runs (below); re-entering one on the stack has no
   fixpoint, so the run fails listing the cycle;
4. **resolve the image** — a `docker://<ref>` is used directly; one of our git
   images is converted to a real image, pushed to the registry, and run by
   digest (see [git images](#git-images));
5. **dispatch to a runner** — the job is matched against the hanging
   `/runner/poll`s (a runner's required args are name → oid pairs the args
   tree's top level must equal; most specific match wins, so a warm runner
   already running this image beats the generic `caos-runnerd`, which starts a
   fresh container `/bin/caos runner --job=<json>`);
6. the runner posts back either the result, `"<type> <hash>"`, or a
   **promise**, `"promise <hash>"`: a map-then continuation the worker recorded
   instead of a value (see
   [map-then](#map-then-sub-computations-without-blocking)). The worker has
   already moved on; the server **resolves** the promise — running `map` over
   the children in parallel, then `then` — through this same pipeline, so
   sub-runs are cached, cycle-checked, and may themselves promise;
7. **cache** the resolved result, and for an **external** run (one that arrived
   over HTTP) pin `refs/caos/res/<reqHash>` at it, for durability and as a
   fetch/watch point. Sub-runs set no ref.

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
straight from `/cas/args/image`, the request's reserved entry — as the mapper,
so each child gets the same treatment, with no std lookup and for any git
image (a rustc-built worker as much as a builtin) — and each child may itself
promise. Because a worker either computes a value or *describes* the remaining
work, only server threads ever wait; a bounded runner pool always drains. See
`design/map-then.md` for the full argument.

### Caching

Results, converted images, and built layers are cached in Redis
(`caos:result:<reqHash>`, `caos:image:<git-hash>`, `caos:layer:<tree-hash>`).
A hit on the result key skips the container entirely (logged `cache hit …` vs
`cache miss …`). Redis is best-effort: if it's unreachable the server logs and
runs uncached. There are no locks yet, so two identical cold-cache requests may
both run.

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
  - `talk` / `chat` — agent conversations over the harness
    (design/agent-harness.md); `caos talk "<prompt>"` is the everyday form.

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

`caos-cli run [--trace[=<file|->]] [--trace-id=<id>] <image> [output] -- [--name=value | --name:@=path …]` (the
blocking, user-facing run):

1. assembles the args into a git **tree** — including the `<image>` under a
   reserved `image` entry (see [arguments](#arguments-literals-and-paths));
2. bundles `{args, std, salt}` into a content-addressed **request object**
   (`reqHash`), where `std` is the standard library in effect (resolved from
   `refs/caos/std`, see [built-ins](#built-ins-casstd));
3. gets the request onto the server — one negotiated `git push` to
   `refs/caos/req/<reqHash>`, whose reachable graph includes any embedded
   git-image tree, so the image needs no separate push;
4. calls `/run?req=<reqHash>`; the server resolves any promises before
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
caos-cli run --trace=trace.jsonl <image> <result-path> -- --input=value
caos-cli run --trace <image> <result-path> -- --input=value
```

Traces are live-only and discarded when the run ends. Trace ids do not affect
request or cache identity.

The worker-side `caos map-then <in> -- [--map=<image>] [--then=<image>]` is a
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

`<image>` (and a `--map`/`--then` value) is a **git image by default**: a bare
git hash (e.g. an `import-image` output or a `caos curry` ref), or — on
`caos-cli` — a `/cas/std/<name>` builtin resolved against the published
library. Inside a worker it can also be any `/cas` path, resolved to the hash
recorded on it. An **ordinary docker image** is written `docker://<ref>`.

### Arguments: literals and paths

An argument is a **literal** or a **path**, chosen by the *operator* — not by
sniffing the value — so a value is never misread and may contain anything (no
escaping):

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

The grammar is `--name[:type]=value` and extensible: `@` (path) and `commit`
are the types today, leaving room for more. The worker `caos` has no host
filesystem (only `/cas`), so a non-`/cas` path there is an error.

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
- `curry <image> -- [--name=value | --name:@=path …]` (both clients) — bind some
  args to an image, printing a ref to the curried image. It's a small
  content-addressed tree (`base`, `args`, a `.caos-curry` marker); `run`/`curry`
  expand it — the CLI for its own calls, the server when a curried `map`/`then`
  runs (call args win, and the base is folded into the args tree as its
  `image` entry) — so a request only ever carries a plain args tree. Currying
  flattens, so it's canonical. On `caos-cli`, path args are host paths to
  ingest, or `/cas/std/<name>` builtin refs.
- `runner --job=<json>` (`caos`) — the container runner; see below.

### `runner`

`caos runner --job=<json>` runs jobs inside the container until an idle TTL
passes (see `design/runner-protocol.md`). Per job:

1. **unpack** — fetch the request tree named by the job's `req` and read its
   `args`/`std`/`salt`;
2. **set up** — wipe and recreate `/cas`, root-owned, and verify xattrs;
   materialize the args at `/cas/args` and the standard library at `/cas/std`;
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

So a `/worker` reads inputs from `/cas/args`, reaches built-ins at `/cas/std`,
and writes its result to `/cas/out`.

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

A worker image is built `FROM` `caos-worker-base` (keeping `/bin/caos` as the
entrypoint) and adds a `/worker` that reads `/cas/args` and writes `/cas/out`.
The Rust workers share `worker-common` (arg helpers, `caos`/`map_then`/`caos
curry` wrappers, result staging).

- **`worker-hello`** — a leaf example: gathers its `/cas/args` entries into a
  result tree.
- **`worker-file-count`** — counts the leaf files under `--in`, recursing with
  itself through map-then: a tree records `{in, map: file-count, then:
  file-count}` and exits; called back with `--children` it sums the counts; a
  file counts as `1`. One image, three positions — the shape any structural
  fold takes here. Identical subtrees are memoized, so a count is incremental
  in the changed nodes; siblings count in parallel.
- **`worker-dirs-only`** — keeps only a node's directory children, dropping
  files. Compose by filtering first and recursing over the result.
- **`worker-deep-deps`** — computes transitive dependencies by self-recursion:
  `deepen` resolves a package's `DEPS` against the map (pure CAS linking) and
  map-thens *itself* over the resolved deps, finishing with a node builder
  keyed only on the package and its subgraph — so recompute is O(changed
  package + its dependents).
- **`worker-rustc`** — compiles a Rust source file into a new worker image:
  given `--src` and a base worker image (`--base`, usually curried in), it builds
  static-musl, linking the vendored `worker-common`, and emits a git-docker
  worker image. So building a worker is itself a (memoized) worker.

### Built-ins (`/cas/std`)

The standard library is a `{name: git-docker-image}` tree reached by workers as
`/cas/std/<name>`. It's published to the server under `refs/caos/std` by
`./build-builtins.sh` (which imports each worker image into a client repo and
`git push`es the assembled tree — one push uploads every referenced image).
`caos-cli run` resolves a `/cas/std/<name>` image against this library (fetching
`refs/caos/std` from the server if needed) and threads its tree hash through as
the request's `std`.

Because `std` is part of every request (hence every cache key), bumping the
built-ins recomputes everything that could reach them — coarse but correct. The
name→hash binding lives outside any worker (in the request), so it's captured in
the cache key, never hidden inside a memoized computation.

```bash
./build-builtins.sh                 # publish all built-ins to refs/caos/std
./build-builtins.sh file-count deep-deps  # publish a subset
```

## Local testing

Run the dev stack with `caosd` (`nix run .#caosd -- <cmd>` in this repo, or just
`caosd <cmd>` wherever `caos-tools` is on PATH — it works from any directory,
including a tree that only imports this flake):

```bash
caosd up      # bring the stack up + publish all of std, then return
caosd logs    # follow the running stack's logs (Ctrl-C returns; stack stays up)
caosd down    # stop it (Redis + registry volumes and the server repo are kept)
caosd reset   # stop and wipe those volumes + the server repo for a clean slate
```

`caosd up` is idempotent and fast on a warm stack (~3s: images already loaded,
the std publish is a cache hit), so re-running it just reconverges the stack to
current. It loads the `caos-server`/`caos-runnerd` images and brings up four
services via `docker compose`: `caos-server` (`:9090`, its dedicated bare repo
mounted at `/git`), `caos-runnerd` (the generic runner, with the docker socket),
`caos-redis`, and `caos-registry`. Redis and the registry persist across restarts
(named volumes); the server's bare repo persists under `CAOS_DATA`.

[Tilt](https://tilt.dev) is still pinned in the dev shell for its auto-rebuild
loop (`tilt up` rebuilds an image when its sources change; UI at
`http://localhost:10350`), but `caosd` is the supported way to run the stack and
tilt is slated for removal.

Integration tests are self-contained — each `run.sh` does `caosd up` itself
(bringing the stack up and publishing std), so you don't need a stack running
first. `run-all.sh` runs the whole suite; the first test cold-starts the stack
and the rest reuse it warm.

```bash
tests/run-all.sh                # every test below, stack brought up once

tests/run.sh tests/deep-deps    # promise recursion: correctness, DAG sharing,
                                # incrementality, cycle detection
tests/run.sh tests/file-count   # self-recursive count over map-then
tests/run.sh tests/dirs-only    # filter worker; filter-then-count composition
tests/run.sh tests/rust-worker  # rustc builder: source -> worker image -> run
tests/run.sh tests/symlinks     # symlinks survive the round trip into /cas
tests/run.sh tests/untracked    # only git-tracked paths are ingested
```

`tests/run.sh` builds `.#caos-cli`, publishes the builtins the test asks for,
and sets up a throwaway client repo with the server as its `caos` remote. A
test is a `cli.sh` run on the host in that repo, driving computations through
`caos-cli` (the one place blocking runs still exist). A test whose assertions
are about what a *worker* sees in a real `/cas` (symlinks, untracked) launches
a bash worker itself, with the worker-side checks in a `check.sh`.

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
- **Cleanup (dev)**: `refs/caos/req/*` and `refs/caos/res/*` accumulate on the
  server repo (content-addressed, so they dedup); a real deployment should expire
  them by age and `git gc`.
