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

          # Shared across deps + both crates so crane keys the dep cache the
          # same way every time.
          pname = "caos-workspace";
          version = "0.1.0";

          CARGO_BUILD_TARGET = muslTarget;

          # Native build inputs / runtime libs go here as the project grows,
          # e.g. pkgs.openssl + pkgs.pkg-config for TLS. Note: C deps would
          # need a musl cross-toolchain to stay static. (object-server's gix
          # uses default-features = false, so it stays pure-Rust / static.)
          # buildInputs = [ ];
          # nativeBuildInputs = [ ];
        };

        # Build all workspace dependencies once and cache them separately from
        # the crates — this is crane's key win for fast incremental rebuilds,
        # and both binaries below share this single dep build.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # One member of the workspace. `--package` scopes the build so each
        # output contains only that crate's binary (keeps each image minimal).
        crateBin =
          pname:
          craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts pname;
              cargoExtraArgs = "--package ${pname}";
              # The package builds only its own binary, so nothing else lands
              # in the output's /bin.
              doCheck = false;
            }
          );

        client = crateBin "client";
        object-server = crateBin "object-server";

        # Minimal images: each contains *only* its static binary — no shell, no
        # libc, no /nix/store. Crates are unprefixed (client, object-server) but
        # the published image names carry a `caos-` prefix.
        # NOTE: Docker images are Linux-only; build these on Linux (or via a
        # remote/linux builder on macOS).

        # The client crate's binary is `client` everywhere except inside its
        # image, where it's exposed as `/bin/caos`. This root also carries the
        # empty `/cas` directory the binary materializes objects into.
        clientImageRoot = pkgs.runCommand "caos-client-root" { } ''
          mkdir -p $out/bin $out/cas
          cp ${client}/bin/client $out/bin/caos
        '';
        clientImage = pkgs.dockerTools.buildImage {
          name = "caos-client";
          tag = "latest";
          copyToRoot = [ clientImageRoot ];
          config = {
            Cmd = [ "/bin/caos" ];
          };
        };

        # Run with the git repo bind-mounted at /git, e.g.
        #   docker run --rm -p 8080:8080 -v /path/to/repo:/git caos-object-server
        objectServerImage = pkgs.dockerTools.buildImage {
          name = "caos-object-server";
          tag = "latest";
          copyToRoot = [ object-server ];
          config = {
            Cmd = [ "/bin/object-server" ];
            ExposedPorts = {
              "8080/tcp" = { };
            };
          };
        };

        # A testing image: caos-client plus an ordinary interactive shell
        # (bash + coreutils + curl) so you can poke at /cas and the object
        # server by hand. Not minimal — for debugging, not production.
        clientBashContents = [
          clientImageRoot
          pkgs.bashInteractive
          pkgs.coreutils
          pkgs.curl
        ];
        clientBashConfig = {
          Cmd = [ "/bin/bash" ];
          Env = [
            "PATH=/bin"
            # Convenience default for the usual docker-compose service name;
            # override with `-e CAOS_OBJECT_SERVER_URL=...`.
            "CAOS_OBJECT_SERVER_URL=http://caos-object-server:8080"
          ];
        };
        clientBashImage = pkgs.dockerTools.buildImage {
          name = "caos-client-bash";
          tag = "latest";
          copyToRoot = clientBashContents;
          config = clientBashConfig;
        };

        # `nix run .#load-<name>` builds the image and pipes it straight into the
        # local docker daemon — build + `docker load` in one go. Uses
        # streamLayeredImage so nothing big is written to the Nix store; the
        # layers are streamed directly to docker. `docker` is taken from PATH.
        loadImage =
          { name, contents, config ? { } }:
          let
            stream = pkgs.dockerTools.streamLayeredImage {
              inherit name config contents;
              tag = "latest";
            };
          in
          pkgs.writeShellApplication {
            name = "load-${name}";
            text = ''
              ${stream} | docker load
            '';
          };

        loadClient = loadImage {
          name = "caos-client";
          contents = [ clientImageRoot ];
          config.Cmd = [ "/bin/caos" ];
        };
        loadObjectServer = loadImage {
          name = "caos-object-server";
          contents = [ object-server ];
          config = {
            Cmd = [ "/bin/object-server" ];
            ExposedPorts = {
              "8080/tcp" = { };
            };
          };
        };
        loadClientBash = loadImage {
          name = "caos-client-bash";
          contents = clientBashContents;
          config = clientBashConfig;
        };
      in
      {
        packages = {
          default = client;
          inherit client object-server;

          # Image tarballs (build with `nix build`, then `docker load < result`).
          caos-client-docker = clientImage;
          caos-object-server-docker = objectServerImage;
          caos-client-bash-docker = clientBashImage;
        };

        apps = {
          # Build the image and load it into the local docker daemon in one go.
          load-caos-client = {
            type = "app";
            program = "${loadClient}/bin/load-caos-client";
          };
          load-caos-object-server = {
            type = "app";
            program = "${loadObjectServer}/bin/load-caos-object-server";
          };
          load-caos-client-bash = {
            type = "app";
            program = "${loadClientBash}/bin/load-caos-client-bash";
          };
        };

        checks = {
          inherit client object-server;

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
