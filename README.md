# caos

A Cargo workspace of small Rust binaries, each packaged into a minimal Docker
image with Nix.

The whole environment — Rust toolchain, build, and images — is defined by the
Nix flake, so builds are reproducible and consistent across machines.

| Crate | Image | What it is |
|---|---|---|
| `client` | `caos-worker-base` | CLI that fetches objects from the object server (see below). Exposed as `caos` inside the image. |
| `object-server` | `caos-object-server` | HTTP daemon over a git object database (see below). |
| `compute-server` | `caos-compute-server` | HTTP daemon that runs a worker image over an args tree and returns its result hash (see below). |

## Prerequisites

- [Nix](https://nixos.org/download) with flakes enabled.
- Docker, to load and run the image.

No Rust toolchain needs to be installed system-wide; the flake pins it.

## Layout

| Path | Purpose |
|---|---|
| `flake.nix` | Dev shell, binary packages, and Docker images — all from one pinned toolchain |
| `rust-toolchain.toml` | Pins the compiler (`stable` + clippy/rustfmt) and the static `musl` target |
| `Cargo.toml` | Workspace root (members + shared release profile) |
| `crates/client/` | The `client` crate → `client` binary |
| `crates/object-server/` | The `object-server` crate → `object-server` binary |
| `crates/compute-server/` | The `compute-server` crate → `compute-server` binary |
| `Cargo.lock` | Pinned dependency versions (required for reproducible Nix builds) |

## Development

Enter a shell with the pinned `rustc`, `cargo`, `clippy`, `rustfmt`, plus
`rust-analyzer` and `cargo-watch`:

```bash
nix develop
```

Inside it, use Cargo as normal (`cargo build`, `cargo run`, `cargo test`).

Run lint, format, and test checks the same way CI would:

```bash
nix flake check
```

> Nix flakes only see files tracked by git. After adding a new file
> (e.g. a new source module), `git add` it before building.

## Building the binaries

```bash
nix build .#client            # output at ./result/bin/client
nix build .#object-server     # output at ./result/bin/object-server
nix build .#compute-server    # output at ./result/bin/compute-server
```

Each binary is statically linked against `musl`, so it has no shared-library
dependencies.

## Building the Docker images

The crates are unprefixed, but the images they produce carry a `caos-` prefix.

```bash
nix build .#caos-worker-base-docker         # image tarball at ./result
nix build .#caos-object-server-docker
nix build .#caos-compute-server-docker
nix build .#caos-worker-hello-docker
nix build .#caos-worker-fold-docker
nix build .#caos-worker-file-count-docker

docker load < result                   # loads e.g. caos-object-server:latest
```

Or build and load into the local docker daemon in one go (streams the image
straight to `docker load`, nothing large written to the Nix store):

```bash
nix run .#load-caos-worker-base
nix run .#load-caos-object-server
nix run .#load-caos-compute-server
nix run .#load-caos-worker-hello
nix run .#load-caos-worker-fold
nix run .#load-caos-worker-file-count
```

The `caos-worker-base` and `caos-object-server` images contain **only** their static
binary under `/bin` — no shell, no libc, no package manager, no `/nix/store`.
The `caos-worker-base` image exposes the binary as `/bin/caos` and runs
`caos entrypoint` (which creates `/cas` at startup — see below). On the worker
images `/bin/caos` is **setuid-root** and there's a `worker` user (uid 1000) plus
a world-writable `/tmp`, so `entrypoint` can run the worker unprivileged while
still letting it reach the root-owned `/cas` through `caos` — see
[Permissions](#permissions-load-before-read-and-no-tampering).

There's also a `caos-worker-bash` image (`.#caos-worker-bash-docker`,
`.#load-caos-worker-bash`) for interactive testing: it's the `caos-worker-base`
root plus `bash`, `coreutils`, and `curl`. Like the other workers it runs
`caos entrypoint`, which sets up `/cas` and runs `/worker` — here `/worker` just
drops you into an interactive shell (and stores an empty `/cas/out` on exit, so
`caos entrypoint` doesn't error if you didn't leave a result). Run it with the
helper script, which wires up the daemon URLs:

```bash
nix run .#load-caos-worker-bash
./run-worker-bash.sh
# inside: caos get-hash <hash> /cas/foo
```

The helper also accepts `--name=value` args, like `caos run`: a value that names
an existing path (relative to your current directory) is stored from the
filesystem and referenced by its git hash; anything else becomes a literal
string. It assembles them into an args tree and starts the container with
`--args=<hash>`, so they land under `/cas/args`:

```bash
./run-worker-bash.sh --greeting=hi --conf=Cargo.toml --src=crates/client
# inside: caos get /cas/args/conf && cat /cas/args/conf
```

When you're running against the default (Tilt) object server — which is backed by
this repo's `.git` — the helper builds the tree with git plumbing directly in the
repo: a **clean, tracked path reuses the hash git already has** (a single
`git ls-tree`, no re-read or upload), so passing a large unchanged directory is
effectively free; only dirty/untracked paths (and literal strings) are hashed in.
It never touches HEAD, the index, or commits. Against any other object server it
falls back to `caos build-args`, which uploads the content through that server.

> Docker images are Linux-only. On macOS, build the `*-docker` outputs via a
> remote or linux builder; the binaries and dev shell build fine natively.

## object-server

An HTTP daemon (`crates/object-server`) that reads and writes git objects in a
repository **mounted at `/git`**, using [gitoxide](https://github.com/GitoxideLabs/gitoxide)
(`gix`). Objects cross the wire in git's native **serialized** form —
`<type> <size>\0<content>`, uncompressed (the same bytes git hashes). Two
endpoints:

| Request | Behaviour |
|---|---|
| `GET /object/<hash>` | Return the serialized object with that hash. `400` if the hash is malformed, `404` if it's absent. |
| `POST /object/` | Store the serialized object in the body (its type and size come from the header) and return git's hash for it (hex). Validates the header, and a `tree` body must be valid tree encoding. Content-addressed, so it's idempotent. |

Run the image with the repo bind-mounted at `/git`:

```bash
docker run --rm -p 8080:80 \
  -v /path/to/repo:/git \
  caos-object-server:latest
```

Storing is normally driven by `caos put`, which frames objects for you. By hand,
the body must be a serialized object, e.g. a blob:

```bash
# Build "blob <size>\0<content>" and POST it; prints the git hash.
printf 'blob 6\0hello\n' | curl -s --data-binary @- \
  http://localhost:8080/object/

# Read it back (returns the serialized object, header included):
curl -s "http://localhost:8080/object/<hash>"
```

The listen address (`OBJECT_SERVER_ADDR`, default `0.0.0.0:80`) and repo path
(`OBJECT_SERVER_GIT_DIR`, default `/git`) are overridable via environment
variables — handy for running outside a container.

## client (`caos`)

The `client` crate (`crates/client`) builds a CLI exposed as `caos` inside its
image. It finds the object server via `$CAOS_OBJECT_SERVER_URL` (and, for
`caos run`, the compute server via `$CAOS_COMPUTE_SERVER_URL`) and materializes
objects under `/cas`.

```text
caos get-hash <hash> <path>   # materialize a given hash at a CAS path
caos get <path>               # expand a placeholder already in /cas
caos put <src-path> <cas-path># store an outside path and record it in /cas
caos run <image> <out> -- ... # run an image on the compute server (see below)
caos build-args [--name=value ...]
                              # print the hash of an args tree (paths from disk,
                              # else literals); used by ./run-worker-bash.sh
caos entrypoint [--args=<hash>]
                              # container entrypoint: set up, run /worker, hash /cas/out
```

**`get-hash <hash> <path>`** — `<path>` must be a **direct child of `/cas`**
(e.g. `/cas/foo`). The object at `<hash>` is fetched with
`GET <url>/object/<hash>`, parsed with
[gitoxide](https://github.com/GitoxideLabs/gitoxide), and:

- a **blob** is written verbatim to `<path>`;
- a **tree** creates the directory `<path>` plus one empty placeholder per
  entry — a **directory** for subtree entries, a **file** otherwise.

(The server returns the serialized object, so its `<type>` header tells the
client whether it's a blob or a tree — no guessing.)

**`get <path>`** — `<path>` may be anywhere inside `/cas` (any depth) and must
already exist. `caos` reads the hash recorded on it (see below), fetches that
object, and expands it in place: an empty **file** is replaced with the blob's
content; an empty **directory** is filled with the tree's entry placeholders.
Together with `get-hash` this lets you lazily drill down a tree one level at a
time — `get-hash` the root, then `get` whichever child you want to expand.

**`put <src-path> <cas-path>`** — the inverse: recursively store a path from
*outside* the CAS into the object server (`POST /object/`), then record the
result at `<cas-path>` (a direct child of `/cas`, like `get-hash`). Files become
blobs and directories become trees. A symlink that resolves to something already
in the CAS is **not** re-read — its recorded hash is reused, so shared content is
stored once.

Files become real git **blobs** and directories real git **trees** — each
`POST`ed as a serialized object — so the hashes are genuine git object hashes; a
`put` directory's hash equals what `git write-tree` would produce.

**`caos run <image> <output-cas-path> -- [--name=value ...]`** — the host side of
a compute step. It assembles the `--name=value` args into a git **tree** stored
in the object server (never written to the filesystem):

- each `--name=value` becomes a tree entry `name`;
- a `value` that is a path inside `/cas` **must exist**, and its entry references
  the object that path was materialized from (its recorded hash) — so inputs are
  passed by reference, not re-uploaded;
- any other `value` is stored verbatim as a blob.

It then asks the compute server (`$CAOS_COMPUTE_SERVER_URL`) to run `<image>`
over that args tree and materializes the returned result hash at
`<output-cas-path>` (a direct child of `/cas`, like `get-hash`). The image is
passed through to the compute server untouched.

**`caos entrypoint [--args=<hash>]`** — the container's entrypoint, tying it
together for a single compute step:

1. **set up** — delete the CAS directory and recreate it empty (**fails** if it
   can't), owned by root and not writable by anyone else, and verify it supports
   xattrs;
2. **load args** — if `--args=<hash>` is given, materialize that object at
   `/cas/args` (exactly like `get-hash <hash> /cas/args`), so the worker can read
   its inputs there;
3. **run `/worker`** — the binary a downstream image is expected to provide,
   dropped to the unprivileged `worker` user (uid/gid 1000) so it can't tamper
   with the root-owned `/cas` directly — only through the setuid-root `caos`.
   `entrypoint` itself stays root, to tear `/cas` down afterwards. The worker's
   stdout is redirected to stderr so the container's stdout stays clean;
4. **report** — print the hash recorded on `/cas/out`. Everything under `/cas`
   got there via `get`/`put`, which already tag each path with its
   `user.caos.hash`, so this is just a fast xattr read — no re-hashing;
5. **tear down** — delete the CAS directory.

So `/worker` typically reads its inputs from `/cas/args`, computes its result,
and writes it to `/cas/out` with `caos put` (or `get`); the printed hash is the
address of that result. The `caos-worker-base` image runs `caos entrypoint` as its
entrypoint, so to make a compute image you build one that adds a `/worker`:

```bash
docker run --rm \
  -e CAOS_OBJECT_SERVER_URL=http://caos-object-server \
  your-worker-image:latest \
  --args=<args-tree-hash>       # /worker must leave its result at /cas/out
```

### Path → hash mapping

Every materialized path records where it came from in the `user.caos.hash`
extended attribute: the top-level path gets `<hash>`, and each child of a tree
gets that entry's own oid (so deeper paths are covered too). This is the on-disk,
per-path mapping from CAS paths back to hashes.

```bash
getfattr -n user.caos.hash --only-values /cas/foo
```

Paths are written atomically (build in a temp sibling, set the xattr, then
`rename` into place), so concurrent runs never see a half-written path or one
missing its hash — no locking needed. On startup `caos` probes the CAS directory
and exits with a clear error if its filesystem doesn't support `user.*` xattrs
(e.g. tmpfs on older kernels, or some overlay setups).

`CAOS_CAS_DIR` (default `/cas`) overrides the CAS directory — handy for running
outside a container.

### Permissions: load-before-read, and no tampering

`/cas` is locked down so a worker can only see what it has explicitly fetched,
and can't alter what's there. Two rules, both enforced by file permissions
(everything under `/cas` is owned by root):

- **Nothing is readable until it's fetched.** Expanding a tree with `get`/
  `get-hash` leaves one *placeholder* per entry — an empty file or directory that
  records its hash but holds no content yet. Placeholders are owner-only
  (`r--------` / `r-x------`), so the worker can't read data it hasn't loaded by
  accident. `get` on a placeholder fetches its content and makes it
  world-readable (`r--r--r--` for a blob, `r-xr-xr-x` for a tree, whose own
  entries are again placeholders). This means you load a tree one level at a
  time, on purpose.
- **The worker can't tamper with `/cas`.** It runs as the unprivileged `worker`
  user, while `/cas` and everything in it is root-owned and not writable by
  others. The worker mutates `/cas` *only* through `caos`, which is **setuid-root**
  inside the image: `get`/`put`/`run` materialize content faithfully (and record
  its hash), but the worker has no other way to write there. (The `caos` binary
  is static, so setuid carries no dynamic-linker attack surface.)

Both rules relax outside the container: with `CAOS_CAS_DIR` pointing at a normal
directory you own, the owner-only modes still let *you* read placeholders' hashes
to expand them, so the round-trip works without root or setuid.

## compute-server

An HTTP daemon (`crates/compute-server`) that runs one containerized compute step
per request. It serves requests **concurrently — one thread per request** —
which is required, not just an optimization: a worker can call back into the
compute server (the fold worker recurses via `caos run`), and that nested request
must be served while the parent's request is still blocked waiting on the `docker
run` it spawned. A serial loop, or any thread pool shallower than the deepest
tree, would deadlock. One endpoint:

| Request | Behaviour |
|---|---|
| `GET /run?image=<image>&args=<hash>` | Return the result hash for running `<image>` over the args tree `<hash>` — from the Redis cache if present, otherwise by running the container and caching the result. `400` for a missing/invalid parameter, `500` if the worker container fails. |

It runs the image by shelling out to the `docker` CLI, forcing the caos
entrypoint so the image's own entrypoint/command don't matter:

```text
docker run --rm --network <net> \
  -e CAOS_OBJECT_SERVER_URL=<url> -e CAOS_COMPUTE_SERVER_URL=<url> \
  --entrypoint /bin/caos <image> entrypoint --args=<hash>
```

`caos entrypoint` populates `/cas/args` from `<hash>`, runs `/worker`, and prints
the hash of `/cas/out` on its stdout — which `docker run` forwards, so the
container's stdout *is* the result hash. So any image that carries `/bin/caos`
and a `/worker` is a valid compute image.

Both daemon URLs are injected into the worker so it can reach the object server
and — for a worker that itself calls `caos run`, like the fold worker — call back
into the compute server. (Workers bake in no URLs of their own.)

Because it drives Docker, the `caos-compute-server` image is **not** minimal — it
bundles the `docker` client and expects the host's docker socket bind-mounted.
The worker containers it spawns join `<net>` so they resolve the daemons by
name:

```bash
docker run --rm -p 9090:80 \
  --network caos-net \
  -v /var/run/docker.sock:/var/run/docker.sock \
  caos-compute-server:latest
```

Overridable via environment: `COMPUTE_SERVER_ADDR` (default `0.0.0.0:80`),
`CAOS_DOCKER_NETWORK` (default `caos-net`), `CAOS_OBJECT_SERVER_URL` (default
`http://caos-object-server`, passed into each worker), `CAOS_COMPUTE_SERVER_URL`
(default `http://caos-compute-server`, our own address passed into each worker so
it can call back), `CAOS_DOCKER_BIN` (default `docker`), and `CAOS_REDIS_ADDR`
(default `caos-redis:6379`).

### Caching

Results are cached in Redis. The key is the image plus the args-tree hash and the
value is the result hash, so an identical request skips the container entirely —
the compute server logs `cache hit …` instead of `cache miss …; running worker`.
Redis is best-effort: if it's unreachable the server logs the error and runs
uncached, so a missing Redis never fails a request. There are no locks yet, so
two identical requests racing a cold cache may both run the work.

### Writing a worker

The base `caos-worker-base` image bakes in **no** `/worker`. A worker image is built
`FROM` it (so it keeps `/bin/caos` as the entrypoint) and adds a `/worker` that
reads its inputs from `/cas/args` and writes its result to `/cas/out` (with
`caos put`/`get`).

The **`caos-worker-hello`** image (`.#caos-worker-hello-docker`,
`.#load-caos-worker-hello`) is a real, runnable example: caos + bash + coreutils
with a `/worker` that copies each `/cas/args` entry into a result directory
(plus a small `receipt`) and stores it at `/cas/out`. So:

```bash
caos put /some/file /cas/in
caos run caos-worker-hello:latest /cas/out -- --in=/cas/in --greeting=hi
caos get /cas/out/greeting && cat /cas/out/greeting   # => hi
```

(The debugging `caos-worker-bash` image's `/worker` drops you into an interactive
shell instead of computing a result — handy for poking around, not a real worker.)

#### A recursive worker: `caos-worker-fold`

The **`caos-worker-fold`** image (`.#caos-worker-fold-docker`,
`.#load-caos-worker-fold`) is a worker whose `/worker` itself calls `caos run` —
both to invoke another image and to recurse into itself. It's a *fold*
(catamorphism) over a CAS tree, taking two args:

- `func` — the worker image to apply (the "algebra");
- `in` — the file or tree to fold over.

Given a **file**, it runs `func` on it. Given a **tree**, it folds each child
with itself (the same `func`), assembles the results into a tree with the
original child names, then runs `func` on that tree. Like every worker, the
applied image takes its single input as `--in`, and the final tree is left at
`/cas/out`:

```bash
caos put /some/tree /cas/in
caos run caos-worker-fold:latest /cas/out -- \
  --func=caos-worker-hello:latest --in=/cas/in
```

Because it drives the compute server itself, the image relies on
`CAOS_COMPUTE_SERVER_URL` (injected into the worker by the compute server, along
with `CAOS_OBJECT_SERVER_URL`) and learns its own name, for the recursive call,
from `CAOS_FOLD_IMAGE`. Each sub-fold is a normal compute step, so identical
subtrees are memoized by the Redis cache.

#### A fold algebra: `caos-worker-file-count`

The **`caos-worker-file-count`** image (`.#caos-worker-file-count-docker`,
`.#load-caos-worker-file-count`) is a small leaf worker meant to be driven by
`caos-worker-fold`. Its single input arrives as `--in`:

- a **file** counts as `1`;
- a **directory** (assumed to hold only files, each containing a number — the
  per-child counts `fold` assembles) returns their **sum**.

The result, a blob holding the count, is left at `/cas/out`. On its own:

```bash
printf hi > /tmp/f && caos put /tmp/f /cas/f
caos run caos-worker-file-count:latest /cas/n -- --in=/cas/f
caos get /cas/n && cat /cas/n        # => 1
```

Composed under `fold`, it counts every leaf file in a tree:

```bash
caos put /some/tree /cas/in
caos run caos-worker-fold:latest /cas/out -- \
  --func=caos-worker-file-count:latest --in=/cas/in
caos get /cas/out && cat /cas/out    # => number of files in the tree
```

## Local testing

[Tilt](https://tilt.dev) is pinned in the dev shell. From `nix develop`:

```bash
tilt up      # build images + run the daemons; UI at http://localhost:10350
# press Ctrl-C in the `tilt up` terminal to stop the daemons
```

**Stopping:** Ctrl-C the `tilt up` process — that tears the daemons down. (`tilt
down` does *not*: it only removes Kubernetes / docker-compose resources, and
these daemons are `local_resource`s, which it ignores.) Each daemon installs a
`SIGINT`/`SIGTERM` handler, so Tilt's signal (forwarded through `docker run`)
makes the container exit and `--rm` removes it. Any container left over from a
prior hard kill is reclaimed on the next `tilt up`.

Run a one-shot interactive bash client with: `./run-worker-bash.sh`

Then inside the container, e.g.:

```
caos get-hash <hash> /cas/foo
mkdir -p /tmp && printf hello > /tmp/in
caos put /tmp/in /cas/in
caos run caos-worker-hello:latest /cas/out -- --in=/cas/in --greeting=hi
caos get /cas/out/greeting && cat /cas/out/greeting
```

`./Tiltfile` builds each image with Nix and runs the object server, compute
server, and a Redis cache as containers Tilt supervises (see Stopping above for
teardown). It tracks each image's sources, so an image is **only** rebuilt and
reloaded when its crate (or the flake/lockfiles) changes — editing
`crates/object-server` reloads just that image and restarts just that daemon. It
also creates the `caos-net` network and a git repo for the object server under
the gitignored `.caos-dev/`.

## Notes

- **Toolchain version** is whatever `stable` resolves to against the locked
  `rust-overlay` revision in `flake.lock`. To pin an exact version, set e.g.
  `channel = "1.96.0"` in `rust-toolchain.toml`.
- **Architecture**: the static target is `x86_64-unknown-linux-musl`. On ARM,
  switch both `rust-toolchain.toml` and `muslTarget` in `flake.nix` to
  `aarch64-unknown-linux-musl`.
- **Native (C) dependencies**: adding a crate that links C libraries (e.g.
  `openssl`) requires a `musl` cross-toolchain to keep the binary static.
  See the commented `buildInputs` / `nativeBuildInputs` in `flake.nix`.
