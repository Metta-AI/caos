# caos

A single Rust binary, packaged into a minimal Docker image with Nix.

The whole environment — Rust toolchain, build, and image — is defined by the
Nix flake, so builds are reproducible and consistent across machines.

## Prerequisites

- [Nix](https://nixos.org/download) with flakes enabled.
- Docker, to load and run the image.

No Rust toolchain needs to be installed system-wide; the flake pins it.

## Layout

| Path | Purpose |
|---|---|
| `flake.nix` | Dev shell, binary package, and Docker image — all from one pinned toolchain |
| `rust-toolchain.toml` | Pins the compiler (`stable` + clippy/rustfmt) and the static `musl` target |
| `Cargo.toml`, `src/` | The `caos` crate → single binary |
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

## Building the binary

```bash
nix build .#caos       # output at ./result/bin/caos
./result/bin/caos
```

The binary is statically linked against `musl`, so it has no shared-library
dependencies.

## Building the Docker image

```bash
nix build .#docker             # image tarball at ./result
docker load < result           # loads caos:latest
docker run --rm caos:latest
```

The image contains **only** the static binary at `/bin/caos` — no shell, no
libc, no package manager, no `/nix/store`. It's intended as a base image:
other images can build on it directly.

```dockerfile
FROM caos:latest
# the static binary is available at /bin/caos
```

> Docker images are Linux-only. On macOS, build `.#docker` via a remote or
> linux builder; the binary and dev shell build fine natively.

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
