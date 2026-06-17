{
  description = "caos — a Rust binary, packaged into a small Docker image with Nix";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";

    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      crane,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Toolchain is pinned via ./rust-toolchain.toml + the flake.lock'd
        # rust-overlay revision, so every build uses the same compiler.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        src = craneLib.cleanCargoSource ./.;

        # Build for musl so the binary is fully static (crt-static is on by
        # default for musl targets) — its runtime closure is just itself.
        # Keep this in sync with the target in ./rust-toolchain.toml.
        muslTarget = "x86_64-unknown-linux-musl";

        commonArgs = {
          inherit src;
          strictDeps = true;

          CARGO_BUILD_TARGET = muslTarget;

          # Native build inputs / runtime libs go here as the project grows,
          # e.g. pkgs.openssl + pkgs.pkg-config for TLS. Note: C deps would
          # need a musl cross-toolchain to stay static.
          # buildInputs = [ ];
          # nativeBuildInputs = [ ];
        };

        # Build all dependencies once and cache them separately from the crate
        # itself — this is crane's key win for fast incremental rebuilds.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        caos = craneLib.buildPackage (commonArgs // { inherit cargoArtifacts; });

        # A base image containing *only* the static binary at /bin/caos — no
        # shell, no libc, no /nix/store. Other images can `FROM caos`.
        # NOTE: Docker images are Linux-only; build this on Linux (or via a
        # remote/linux builder on macOS).
        dockerImage = pkgs.dockerTools.buildImage {
          name = "caos";
          tag = "latest";
          copyToRoot = [ caos ];
          config = {
            Cmd = [ "/bin/caos" ];
          };
        };
      in
      {
        packages = {
          default = caos;
          caos = caos;
          docker = dockerImage;
        };

        checks = {
          inherit caos;

          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          fmt = craneLib.cargoFmt { inherit src; };

          test = craneLib.cargoTest (commonArgs // { inherit cargoArtifacts; });
        };

        devShells.default = craneLib.devShell {
          # Brings the pinned toolchain (rustc, cargo, clippy, rustfmt) onto PATH.
          checks = self.checks.${system};
          packages = [
            pkgs.cargo-watch
            pkgs.rust-analyzer
          ];
        };
      }
    );
}
