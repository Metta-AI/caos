# caos

A Cargo workspace of small Rust binaries, each packaged into a minimal Docker
image with Nix.

The whole environment — Rust toolchain, build, and images — is defined by the
Nix flake, so builds are reproducible and consistent across machines.

| Crate | Image | What it is |
|---|---|---|
| `client` | `caos-client` | Placeholder binary. |
| `object-server` | `caos-object-server` | HTTP daemon over a git object database (see below). |

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
```

Each binary is statically linked against `musl`, so it has no shared-library
dependencies.

## Building the Docker images

The crates are unprefixed, but the images they produce carry a `caos-` prefix.

```bash
nix build .#caos-client-docker         # image tarball at ./result
nix build .#caos-object-server-docker

docker load < result                   # loads e.g. caos-object-server:latest
```

Or build and load into the local docker daemon in one go (streams the image
straight to `docker load`, nothing large written to the Nix store):

```bash
nix run .#load-caos-client
nix run .#load-caos-object-server
```

Each image contains **only** its static binary under `/bin` — no shell, no
libc, no package manager, no `/nix/store`.

> Docker images are Linux-only. On macOS, build the `*-docker` outputs via a
> remote or linux builder; the binaries and dev shell build fine natively.

## object-server

An HTTP daemon (`crates/object-server`) that reads and writes git objects in a
repository **mounted at `/git`**, using [gitoxide](https://github.com/GitoxideLabs/gitoxide)
(`gix`). Two endpoints:

| Request | Behaviour |
|---|---|
| `GET /object/<hash>` | Return the raw (decompressed, header-stripped) data of the object with that hash. `400` if the hash is malformed, `404` if it's absent. |
| `POST /object/` | Write the request body into the repo as a blob and return git's hash for it (hex). Content-addressed, so it's idempotent. |

Run the image with the repo bind-mounted at `/git`:

```bash
docker run --rm -p 8080:8080 \
  -v /path/to/repo:/git \
  caos-object-server:latest
```

Then:

```bash
# Store data, get its hash back (matches `git hash-object -w`):
hash=$(curl -s --data-binary @file.bin \
  http://localhost:8080/object/)

# Read it back:
curl -s "http://localhost:8080/object/$hash"
```

The listen address (`OBJECT_SERVER_ADDR`, default `0.0.0.0:8080`) and repo path
(`OBJECT_SERVER_GIT_DIR`, default `/git`) are overridable via environment
variables — handy for running outside a container.

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
