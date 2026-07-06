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
| `worker-hello`, `worker-fold`, `worker-file-count`, `worker-dirs-only`, `worker-deep-deps`, `worker-rustc` | `caos-worker-<name>` | Example/built-in workers. See [workers](#workers). |

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

> Docker images are Linux-only. On macOS, build the `*-docker` outputs via a
> remote/linux builder; the binaries and dev shell build natively.

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
`CAOS_DOCKER_BIN` (`docker`), `CAOS_REDIS_ADDR` (`caos-redis:6379`).

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
  push/fetch. It has no `/cas` and no object-level commands — just two:
  - `run` — compute, with the result checked out to any host path;
  - `import-image` — get a docker image into caos, printing its hash.

`caos-cli` must run inside a git working tree with the server as its `caos`
remote, and `CAOS_SERVER_URL` set (used for `/run` and to fetch results):

```bash
git remote add caos http://localhost:9090
export CAOS_SERVER_URL=http://localhost:9090
```

### The CAS and `/cas`

`/cas` is a **worker** thing — there's no CAS on the host. Inside a worker the
`caos` binary materializes objects under `/cas`, and every materialized path is
tagged with the git hash it came from in the `user.caos.hash` xattr — the
on-disk, per-path mapping from a path back to its hash. Writes are atomic (build
in a temp sibling, set the xattr, `rename` into place), so concurrent runs never
see a half-written path; startup probes that the filesystem supports `user.*`
xattrs.

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

`caos run <image> <output> -- [--name=value | --name:@=path …]` (on `caos-cli`,
`<output>` may be omitted — see step 5):

1. assembles the args into a git **tree** — including the `<image>` under a
   reserved `image` entry (see [arguments](#arguments-literals-and-paths));
2. bundles `{args, std, salt}` into a content-addressed **request object**
   (`reqHash`), where `std` is the standard library in effect (resolved from
   `refs/caos/std`, see [built-ins](#built-ins-casstd));
3. gets the request onto the server — `caos-cli` **pushes** it (one negotiated
   `git push` to `refs/caos/req/<reqHash>`, plus a git image's own objects);
   the worker `caos` had POSTed it via `/object`;
4. calls `/run?req=<reqHash>`;
5. records the result at `<output>`. Here `caos-cli` and the worker `caos`
   differ: `caos-cli` **checks the result out in full** — fetching the object and
   (for a tree) every descendant as ordinary rw files (`0644`/`0755`, git's
   executable bit preserved), so it's readable and editable on the host directly.
   `<output>` is optional on `caos-cli`: with it omitted, a **file** result is
   streamed to **stdout** (handy for `| less` or `> file`); a **tree** result has
   no single stream, so it still needs an `<output>` path. A worker records a
   **typed, tagged placeholder** instead and **fetches nothing**: the result
   stays on the server (read-only CAS modes), and `caos get <output>` pulls the
   bytes on demand if it wants them.

So a result never comes back to a *worker* automatically. A deep fold propagates
hashes up the tree, materializing only the leaves a worker actually reads. The
worker, recursing, references child results by hash (it `link`s the placeholder
and `caos put` reuses the recorded hash — no content needed). Only at the top,
where `caos-cli` returns the final result to the user, is the whole tree pulled
down.

**Failures propagate.** If a worker exits non-zero, `entrypoint` makes the
container fail, the server answers `/run` with `500` carrying the worker's stderr,
and the caller's `run` returns that as an error — so `caos run`/`caos-cli run`
fails (non-zero exit) with the worker's message. This holds at any depth: a
failure deep in a fold travels up through each parent's `caos run` to the
top-level `caos-cli run`. (The run-cycle error is one such case.)

`<image>` is a **git image by default**: a bare git hash (e.g. an `import-image`
output or, in a worker, a `caos curry` ref), or — on `caos-cli` — a
`/cas/std/<name>` builtin resolved against the published library. Inside a worker
it can also be any `/cas` path, resolved to the hash recorded on it. An **ordinary
docker image** is written `docker://<ref>`.

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

The grammar is `--name[:type]=value` and extensible: `@` (path) is the only type
today, leaving room for more. The worker `caos` has no host filesystem (only
`/cas`), so a non-`/cas` path there is an error.

### Other subcommands

`import-image` is the only other `caos-cli` command; the rest are **worker** (`caos`)
commands, operating on `/cas`.

- `import-image <docker-archive>` (`caos-cli`) — store a docker-archive image
  (`nix build .#caos-*-docker` output) as a git-docker tree on the server,
  printing its hash. Used to ingest images into caos so they can be `run` (and to
  assemble the std library — see `build-builtins.sh`).
- `put <src-path> <cas-path>` (`caos`) — store an outside path into the CAS and
  record it at a `/cas` path. Files become blobs, directories trees; a symlink
  into the CAS reuses the recorded hash.
- `curry <image> -- [--name=value | --name:@=path …]` (`caos`) — bind some args to
  an image, printing a ref to the curried image. It's a small content-addressed
  tree (`base`, `args`, a `.caos-curry` marker); `run`/`curry` expand it
  client-side (call args win, and the base is folded into the args tree as its
  `image` entry), so the server only ever sees a plain args tree. Currying
  flattens, so it's canonical.
- `entrypoint [--args=<hash>]` (`caos`) — the container entrypoint; see below.

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

There's no `/cas` outside a container: `caos-cli` never materializes objects
locally — it pushes/fetches git objects and checks a `run` result out as ordinary
files.

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
- **`worker-dirs-only`** — a `pre` algebra: keeps only a node's directory
  children, dropping files. As `fold --pre=dirs-only` it makes the fold recurse
  into subdirectories only — files are never folded as leaves.
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

Both build `.#caos`, set up a throwaway client repo with the server as its `caos`
remote, and drive everything through `caos-cli`.

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
