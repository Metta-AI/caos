# caos

Content-addressed storage and compute over git objects. A worker is just a
container that reads inputs from a content-addressed store (CAS), writes a
result back, and is addressed — inputs, image, and output — entirely by git
hashes. Computations are pure functions of their inputs, so results are
memoized; trees are real git objects, so unchanged data is shared and transfers
are incremental.

It's a Cargo workspace of small static Rust binaries, each packaged into a
minimal Docker image with Nix, and wired together for local dev with Tilt. The
whole environment — toolchain, builds, images — is defined by the Nix flake, so
it's reproducible across machines.

| Crate | Binaries / image | What it is |
|---|---|---|
| `caos` | `caos`, `caos-cli` | One library, two clients. `caos` is the worker-side client (baked setuid into worker images at `/bin/caos`); `caos-cli` is the user-facing client. See [clients](#the-two-clients). |
| `server` | `caos-server` | One daemon: object storage, compute, and a git smart-HTTP transport, over its own repo. See [server](#server). |
| `worker-common` | — | Shared library for the Rust workers. |
| `worker-hello`, `worker-fold`, `worker-file-count`, `worker-deep-deps`, `worker-rustc` | `caos-worker-<name>` | Example/built-in workers. See [workers](#workers). |

## Prerequisites

- [Nix](https://nixos.org/download) with flakes enabled.
- Docker, to load and run the images.

No Rust toolchain is needed system-wide; the flake pins it. Linux and macOS
(Apple Silicon) are both first-class: on macOS everything builds natively — the
Linux images cross-compile (no VM) and run under Docker Desktop or OrbStack.

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
nix build .#caos              # ./result/bin/{caos,caos-cli} — static musl (Linux)
nix build .#server            # ./result/bin/server
nix build .#caos-cli          # the user-facing client, native to the build host
```

The `caos`/`server` binaries are statically linked against `musl` — no
shared-library dependencies. `caos-cli` drives the server from your working tree,
so it runs on the *host* and is built for the host platform; on Linux the musl
`caos-cli` inside `.#caos` is host-runnable too.

Docker images (crates are unprefixed; images carry a `caos-` prefix):

```bash
nix build .#caos-server-docker            # image tarball at ./result
nix build .#caos-worker-base-docker
nix build .#caos-worker-hello-docker      # ...-fold, -file-count, -deep-deps, -rustc, -bash

docker load < result
```

Or build and load into the local docker daemon in one step (streamed, nothing
large written to the Nix store):

```bash
nix run .#load-caos-server
nix run .#load-caos-worker-hello          # load-caos-worker-{base,fold,...}
```

Worker images contain **only** their static `/worker` binary plus a setuid-root
`/bin/caos`, the `worker` user (uid 1000), and a writable `/tmp` — no shell, no
libc, no `/nix/store`. The `caos-server` image is not minimal: it bundles the
`docker` client, `git`, and `tar`, and expects the host's docker socket.

> The images are Linux, but build directly on macOS too — no Linux builder or VM.
> The flake cross-compiles the Rust binaries for the build host's architecture
> with the toolchain's bundled `rust-lld` (so an `aarch64` Mac produces `aarch64`
> images that run natively under Docker Desktop/OrbStack), and the server image's
> general-purpose tools (`git`/`tar`/`docker`) are substituted prebuilt from the
> binary cache rather than built.

## The big picture

- A **server** holds the canonical CAS and runs compute. It exposes three faces
  over one URL: an HTTP object API (`/object`), an HTTP compute trigger
  (`/run`), and a **git smart-HTTP transport** over its own repo.
- A **worker** is a container the server runs. It reaches the server over HTTP,
  reading inputs from and writing results to a per-run `/cas` directory through
  the setuid `caos` binary.
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

It serves requests **concurrently — one thread per request** — which is
required, not just an optimization: a worker can call back into `/run` (the fold
worker recurses), and that nested request must be served while the parent's is
still blocked on the `docker run` it spawned. A serial loop, or a pool shallower
than the deepest tree, would deadlock.

| Request | Behaviour |
|---|---|
| `GET /object/<hash>` | Return the serialized object (`<type> <size>\0<content>`, the bytes git hashes). `400` if malformed, `404` if absent. |
| `POST /object/` | Store the serialized object in the body, return its git hash. Content-addressed, so idempotent. |
| `GET /run?req=<reqHash>[&stack=…]` | Run the request object `<reqHash>` and return `"<type> <hash>"` (the result). See [compute](#compute). |
| `GET /info/refs?service=…`, `POST /git-upload-pack`, `POST /git-receive-pack` | Git smart-HTTP, delegated to `git http-backend` — this is the `caos` remote clients push to and fetch from. |

The git transport is what makes the server a `caos` remote: `git http-backend`
runs `upload-pack`/`receive-pack` over the same `/git` repo, with hooks intact
(so a `post-receive` trigger is a natural future evolution). The dedicated repo
is created with `http.receivepack=true` (to accept pushes) and
`uploadpack.allowAnySHA1InWant=true` (so a client can `git fetch` a result by
its bare hash; `/object` itself never needs that flag).

Environment overrides: `SERVER_ADDR` (`0.0.0.0:80`), `CAOS_GIT_DIR` (`/git`),
`CAOS_DOCKER_NETWORK` (`caos-net`), `CAOS_SERVER_URL` (`http://caos-server`,
injected into each worker), `CAOS_REGISTRY_PUSH_URL`
(`http://caos-registry:5000`), `CAOS_REGISTRY_PULL_HOST` (`localhost:5000`),
`CAOS_DOCKER_BIN` (`docker`), `CAOS_REDIS_ADDR` (`caos-redis:6379`),
`CAOS_WORKER_ENV` (empty) — a comma-separated allowlist of the server's own env
vars to forward into each worker (e.g. `ANTHROPIC_API_KEY` for the `llm-summary`
worker); forwarded via env, never the args tree, so a secret stays out of the
request hash and result cache key.

### Compute

A run **request** is itself a content-addressed git object: a tree
`{image, args, std, salt}` whose hash, `reqHash`, *is* the cache key and the
rendezvous id. `GET /run?req=<reqHash>`:

1. **read** the request tree (`image` ref, `args` tree, `std` tree, `salt`);
2. **cache** lookup in Redis keyed on `reqHash` — a hit returns the cached
   `"<type> <hash>"` and skips everything below;
3. **cycle check** — `&stack=` carries the chain of in-progress `reqHash`es
   (threaded through nested runs via `CAOS_RUN_STACK`); re-entering one on the
   stack has no fixpoint, so the run fails listing the cycle;
4. **resolve the image** — a `docker://<ref>` is used directly; one of our git
   images is converted to a real image, pushed to the registry, and run by
   digest (see [git images](#git-images));
5. **run the container**, forcing `/bin/caos entrypoint --args=<args>` with
   `CAOS_SERVER_URL`, `CAOS_STD`, `CAOS_SALT`, and the child `CAOS_RUN_STACK`
   injected (so `std`/`salt`/stack thread into nested runs);
6. its stdout — `"<type> <hash>"` printed by `entrypoint` — is the result;
7. **cache** it, and for a **top-level** run (empty stack) pin
   `refs/caos/res/<reqHash>` at the result, for durability and as a fetch/watch
   point. Nested runs set no ref.

Results stay on the server. The caller gets back the hash and a type; it does
**not** receive the bytes unless it asks (see [result handling](#requests-and-results)).

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

- **`caos`** (worker-side) talks to the server over **HTTP** (`/object`, `/run`),
  and provides the container `entrypoint`. It's installed **setuid-root** in
  worker images so an unprivileged worker can reach the root-owned `/cas` only
  through it. Subcommands: `get-hash`, `get`, `put`, `run`, `curry`,
  `entrypoint`.
- **`caos-cli`** (user-facing) uses the server as a **`caos` git remote**: it
  builds objects in the local working repo and exchanges them by negotiated
  push/fetch. Subcommands: `get-hash`, `get`, `put`, `import-image`, `resolve`,
  `run`, `curry`. No `entrypoint`.

`caos-cli` must run inside a git working tree with the server as its `caos`
remote, and `CAOS_SERVER_URL` set (used for `/run` and to fetch results):

```bash
git remote add caos http://localhost:9090
export CAOS_SERVER_URL=http://localhost:9090
export CAOS_CAS_DIR=$PWD/.caos-cas      # a local working CAS (see below)
```

### The CAS and `/cas`

Objects are materialized under a CAS directory (`/cas`, or `$CAOS_CAS_DIR`).
`get-hash`/`get`/`put`/`run` all operate there, and every materialized path is
tagged with the git hash it came from in the `user.caos.hash` xattr — the
on-disk, per-path mapping from a path back to its hash. Writes are atomic (build
in a temp sibling, set the xattr, `rename` into place), so concurrent runs never
see a half-written path; startup probes that the filesystem supports `user.*`
xattrs.

`get-hash <hash> <path>` materializes an object at `<path>` (a direct child of
the CAS): a **blob** becomes a file; a **tree** becomes a directory of one-level
**placeholders** (empty, hash-tagged — a dir for subtrees, a file otherwise).
`get [-r|--recursive[=<n>]] <path>` expands a placeholder in place: one level by
default, `<n>` levels, or the whole subtree with `-r`. So you drill down a tree
lazily, one level at a time, and `get -r` is idempotent/resumable.

On a worker, `/cas` is genuinely protected (see [permissions](#permissions-load-before-read-and-no-tampering));
under `caos-cli` it's just a local working directory you own, and the permission
modes are vestigial bookkeeping.

### Requests and results

`caos run <image> <output> -- [--name=value | --name:@=path …]`:

1. assembles the args into a git **tree** (see [arguments](#arguments-literals-and-paths));
2. bundles `{image, args, std, salt}` into a content-addressed **request object**
   (`reqHash`), where `std` is the standard library in effect (resolved from
   `refs/caos/std`, see [built-ins](#built-ins-casstd));
3. gets the request onto the server — `caos-cli` **pushes** it (one negotiated
   `git push` to `refs/caos/req/<reqHash>`, plus a git image's own objects);
   the worker `caos` had POSTed it via `/object`;
4. calls `/run?req=<reqHash>`;
5. records the result at `<output>` as a **typed, tagged placeholder** — and
   **fetches nothing**. The result stays on the server; `caos get <output>`
   pulls the bytes on demand if you want them.

So a result never comes back to the caller automatically. A deep fold propagates
hashes up the tree, materializing only the leaves a worker actually reads. The
worker, recursing, references child results by hash (it `link`s the placeholder
and `caos put` reuses the recorded hash — no content needed).

`<image>` is a **git image by default** (a `/cas` path resolved to its recorded
hash, or a bare git hash — e.g. a `caos curry` ref); an **ordinary docker image**
is written `docker://<ref>`.

### Arguments: literals and paths

An argument is a **literal** or a **path**, chosen by the *operator* — not by
sniffing the value — so a value is never misread and may contain anything (no
escaping):

- `--name=value` → a literal string, stored as a blob;
- `--name:@=path` → a path (the `@` nods to curl/HTTPie). It's resolved doing as
  little work as possible:
  - inside `$CAOS_CAS_DIR` → reference the hash recorded on it (no read);
  - a host path (caos-cli) → ingest via git, reusing git's own objects:
    - **clean + tracked** → reuse the committed hash from `git ls-tree HEAD` — no
      read at all, so a large unchanged directory is effectively free;
    - **dirty file** → `git hash-object -w`;
    - **dirty directory** → copy `.git/index` to a throwaway index and `git add`
      + `write-tree --prefix` there, so only the **changed** files are re-read
      (the stat-cache covers the rest) and your real index is untouched — the
      trick `git stash`/`commit` use;
    - **outside the worktree** → read in full;
  - a **missing** path is an error, not silently a literal.

The grammar is `--name[:type]=value` and extensible: `@` (path) is the only type
today, leaving room for more. The worker `caos` has no host filesystem (only
`/cas`), so a non-`/cas` path there is an error.

### Other subcommands

- `put <src-path> <cas-path>` — store an outside path into the CAS and record it
  at a `/cas` path. Files become blobs, directories trees; a symlink into the CAS
  reuses the recorded hash. (`caos-cli` writes objects to the local repo; the
  worker POSTs them.)
- `import-image <docker-archive> <cas-path>` (`caos-cli`) — store a docker-archive
  image (`nix build .#caos-*-docker` output) as a git-docker tree, printing its
  hash. Used to ingest images into the CAS so they can be `run`.
- `resolve <ref>` (`caos-cli`) — print the tree hash a local git ref points at
  (peeling commits/tags), e.g. `caos resolve refs/caos/std`. Ref name → hash; it
  does **not** hash filesystem paths.
- `curry <image> -- [--name=value | --name:@=path …]` — bind some args to an
  image, printing a ref to the curried image. It's a small content-addressed tree
  (`base`, `args`, a `.caos-curry` marker); `run`/`curry` expand it client-side
  (call args win), so the server only ever sees a plain image + args. Currying
  flattens, so it's canonical.
- `entrypoint [--args=<hash>]` (`caos` only) — the container entrypoint; see below.

### `entrypoint`

`caos entrypoint` ties a single compute step together inside the container:

1. **set up** — wipe and recreate `/cas`, root-owned, and verify xattrs;
2. **load** — if `--args=<hash>`, materialize it at `/cas/args`; if `$CAOS_STD`
   is set, materialize the standard library at `/cas/std`;
3. **run `/worker`** — dropped to the unprivileged `worker` user so it can't
   touch the root-owned `/cas` except through setuid `caos`; `entrypoint` stays
   root to tear down. The worker's stdout is sent to stderr so the container's
   stdout stays clean;
4. **report** — print `"<type> <hash>"` for `/cas/out` (a fast xattr read plus an
   `is_dir` check — no re-hashing). The server returns this to the caller, which
   uses the type to make a correctly-typed result placeholder without fetching;
5. **tear down** — delete `/cas`.

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

Outside a container (`caos-cli`, `CAOS_CAS_DIR` a directory you own) the rules
relax — you own everything, so the modes are just bookkeeping.

## Workers

A worker image is built `FROM` `caos-worker-base` (keeping `/bin/caos` as the
entrypoint) and adds a `/worker` that reads `/cas/args` and writes `/cas/out`.
The Rust workers share `worker-common` (arg helpers, `caos`/`caos run`/`caos
curry` wrappers, result staging).

- **`worker-hello`** — a leaf example: gathers its `/cas/args` entries into a
  result tree.
- **`worker-fold`** — a recursive fold (catamorphism) over a CAS tree, driving
  `caos run` to recurse and to apply two image "functions":
  - `pre` (optional) — applied to `--in` to produce the tree of children to fold
    (default: a tree's own children; a file is a leaf);
  - `post` — applied to `--in` plus `--children` (the folded child results) to
    produce this node's result.

  Identical subtrees are memoized, so a fold is incremental in the changed nodes.
- **`worker-file-count`** — a `post` algebra: a file counts as `1`, a directory
  sums its children's counts.
- **`worker-deep-deps`** — computes transitive dependencies, implemented as a
  curried fold (`pre` resolves a package's deps against a package map; `post`
  assembles the deep-deps tree).
- **`worker-rustc`** — compiles a Rust source file into a new worker image:
  given `--src` and a base worker image (`--base`, usually curried in), it builds
  static-musl, linking the vendored `worker-common`, and emits a git-docker
  worker image. So building a worker is itself a (memoized) worker.

### Built-ins (`/cas/std`)

The standard library is a `{name: git-docker-image}` tree reached by workers as
`/cas/std/<name>`. It's published to the server under `refs/caos/std` by
`./build-builtins.sh` (which imports each worker image into a client repo and
`git push`es the assembled tree — one push uploads every referenced image). A
client `git fetch`es `refs/caos/std`, `caos-cli resolve`s it locally to a tree
hash, and `caos run` threads that hash through as the request's `std`.

Because `std` is part of every request (hence every cache key), bumping the
built-ins recomputes everything that could reach them — coarse but correct. The
name→hash binding lives outside any worker (in the request), so it's captured in
the cache key, never hidden inside a memoized computation.

```bash
./build-builtins.sh                 # publish all built-ins to refs/caos/std
./build-builtins.sh fold deep-deps  # publish a subset
```

## Local testing

[Tilt](https://tilt.dev) is pinned in the dev shell. From `nix develop`:

```bash
tilt up      # build images + run the daemons; UI at http://localhost:10350
```

The `Tiltfile` builds each image with Nix (only when its sources, or the
flake/lockfiles, change), creates the `caos-net` network and the server's
**dedicated bare repo** (`.caos-dev/server-repo.git`, with `http.receivepack`
and `uploadpack.allowAnySHA1InWant`), and runs three daemons: `caos-server`
(`:9090`, with the docker socket and the repo mounted at `/git`), `caos-redis`,
and a `caos-registry`.

**Stopping:** Ctrl-C the `tilt up` process — that tears the daemons down. (`tilt
down` does *not*: the daemons are `local_resource`s, which it ignores.) Each
daemon handles `SIGINT`/`SIGTERM`, so it exits and `--rm` removes it.

Integration tests (require `tilt up` running):

```bash
./test-deep-deps.sh      # deep-deps via /cas/std: correctness, caching, Merkle
                         # incrementality, std-key invalidation, cycle detection
./test-rust-worker.sh    # rustc builder: source -> worker image -> run, memoized
./test-host-path.sh      # a host path passed to `caos run`: content delivered,
                         # clean tracked tree reused, dirty tree hashed incrementally
```

Each builds `.#caos-cli` (the host-native client), sets up a throwaway client repo
with the server as its `caos` remote, and drives everything through `caos-cli`.

## Notes

- **Toolchain version** is whatever `stable` resolves to against the locked
  `rust-overlay` revision in `flake.lock`. Pin an exact version with `channel =
  "1.96.0"` in `rust-toolchain.toml`.
- **Architecture**: the musl target follows the build host's architecture
  (`aarch64-unknown-linux-musl` on Apple Silicon / ARM Linux, `x86_64` elsewhere);
  `rust-toolchain.toml` carries both. macOS cross-links with the toolchain's
  `rust-lld` (see `muslCrossLinker` in `flake.nix`), so no Linux builder is needed.
- **Native (C) dependencies**: a crate linking C libraries (e.g. `openssl`)
  needs a `musl` cross-toolchain to stay static — see the commented
  `buildInputs`/`nativeBuildInputs` in `flake.nix`.
- **Cleanup (dev)**: `refs/caos/req/*` and `refs/caos/res/*` accumulate on the
  server repo (content-addressed, so they dedup); a real deployment should expire
  them by age and `git gc`.
