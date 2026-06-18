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
        compute-server = crateBin "compute-server";

        # Minimal images: each contains *only* its static binary — no shell, no
        # libc, no /nix/store. Crates are unprefixed (client, object-server) but
        # the published image names carry a `caos-` prefix.
        # NOTE: Docker images are Linux-only; build these on Linux (or via a
        # remote/linux builder on macOS).

        # The client crate's binary is `client` everywhere except inside its
        # image, where it's exposed as `/bin/caos`. The `/cas` directory is *not*
        # baked in — `caos entrypoint` creates it at runtime (so a mounted, empty
        # /cas volume works too).
        clientImageRoot = pkgs.runCommand "caos-client-root" { } ''
          mkdir -p $out/bin
          cp ${client}/bin/client $out/bin/caos
        '';
        # The container runs `caos entrypoint <command>`: set up /cas, run the
        # command, then print the hash of /cas/out.
        clientConfig = {
          Entrypoint = [
            "/bin/caos"
            "entrypoint"
          ];
          Env = [ "CAOS_OBJECT_SERVER_URL=http://caos-object-server:8080" ];
        };
        clientImage = pkgs.dockerTools.buildImage {
          name = "caos-client";
          tag = "latest";
          copyToRoot = [ clientImageRoot ];
          config = clientConfig;
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
        # server by hand. Not minimal — for debugging, not production. It runs a
        # plain shell (no entrypoint), so it ships a ready-made /cas to poke at.
        casDir = pkgs.runCommand "caos-cas-dir" { } "mkdir -p $out/cas";
        # The base caos-client image bakes in no /worker; downstream images
        # supply it. This debugging image's /worker is just bash, so running it
        # through `caos entrypoint` (which always runs /worker) drops you in a
        # shell. For a real worker that reads /cas/args and writes /cas/out, see
        # the caos-hello-worker image below.
        workerBash = pkgs.runCommand "caos-worker-bash" { } ''
          mkdir -p $out
          ln -s /bin/bash $out/worker
        '';
        clientBashContents = [
          clientImageRoot
          casDir
          workerBash
          pkgs.bashInteractive
          pkgs.coreutils
          pkgs.curl
        ];
        clientBashConfig = {
          Cmd = [ "/bin/bash" ];
          Env = [
            "PATH=/bin"
            # Convenience defaults for the usual docker-compose service names;
            # override with `-e CAOS_OBJECT_SERVER_URL=...` / `-e CAOS_COMPUTE_SERVER_URL=...`.
            "CAOS_OBJECT_SERVER_URL=http://caos-object-server:8080"
            "CAOS_COMPUTE_SERVER_URL=http://caos-compute-server:9090"
          ];
        };
        clientBashImage = pkgs.dockerTools.buildImage {
          name = "caos-client-bash";
          tag = "latest";
          copyToRoot = clientBashContents;
          config = clientBashConfig;
        };

        # A real, runnable worker image: caos + bash + coreutils, with a /worker
        # that reads its inputs from /cas/args (one file per `--name=value` arg
        # `caos run` passed), copies them into a result directory along with a
        # small receipt, and stores that at /cas/out. The compute server runs it
        # via `caos entrypoint`, which populates /cas/args and runs /worker.
        helloWorkerScript = pkgs.writeTextFile {
          name = "caos-hello-worker-script";
          executable = true;
          destination = "/worker";
          text = ''
            #!/bin/bash
            set -euo pipefail
            echo "hello-worker: reading /cas/args" >&2
            out=/tmp/out
            rm -rf "$out"
            mkdir -p "$out"
            for path in /cas/args/*; do
              name=$(basename "$path")
              caos get "$path"          # expand the placeholder to real content
              cp -r "$path" "$out/$name"
              echo "  saw $name" >&2
            done
            {
              echo "worker ran"
              for path in /cas/args/*; do
                echo "saw $(basename "$path")"
              done
            } > "$out/receipt"
            caos put "$out" /cas/out
          '';
        };
        helloWorkerContents = [
          clientImageRoot
          helloWorkerScript
          pkgs.bashInteractive
          pkgs.coreutils
        ];
        helloWorkerConfig = {
          Entrypoint = [
            "/bin/caos"
            "entrypoint"
          ];
          Env = [
            "PATH=/bin"
            "CAOS_OBJECT_SERVER_URL=http://caos-object-server:8080"
          ];
        };
        helloWorkerImage = pkgs.dockerTools.buildImage {
          name = "caos-hello-worker";
          tag = "latest";
          copyToRoot = helloWorkerContents;
          config = helloWorkerConfig;
        };

        # compute-server runs worker containers by shelling out to the `docker`
        # CLI, so — unlike the minimal images — it bundles the docker client and
        # expects the host's docker socket bind-mounted at /var/run/docker.sock:
        #   docker run --rm --network caos-net -p 9090:9090 \
        #     -v /var/run/docker.sock:/var/run/docker.sock caos-compute-server
        computeServerContents = [
          compute-server
          pkgs.docker-client
        ];
        computeServerConfig = {
          Cmd = [ "/bin/compute-server" ];
          Env = [ "PATH=/bin" ];
          ExposedPorts = {
            "9090/tcp" = { };
          };
        };
        computeServerImage = pkgs.dockerTools.buildImage {
          name = "caos-compute-server";
          tag = "latest";
          copyToRoot = computeServerContents;
          config = computeServerConfig;
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
          config = clientConfig;
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
        loadComputeServer = loadImage {
          name = "caos-compute-server";
          contents = computeServerContents;
          config = computeServerConfig;
        };
        loadHelloWorker = loadImage {
          name = "caos-hello-worker";
          contents = helloWorkerContents;
          config = helloWorkerConfig;
        };
      in
      {
        packages = {
          default = client;
          inherit client object-server compute-server;

          # Image tarballs (build with `nix build`, then `docker load < result`).
          caos-client-docker = clientImage;
          caos-object-server-docker = objectServerImage;
          caos-client-bash-docker = clientBashImage;
          caos-compute-server-docker = computeServerImage;
          caos-hello-worker-docker = helloWorkerImage;
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
          load-caos-compute-server = {
            type = "app";
            program = "${loadComputeServer}/bin/load-caos-compute-server";
          };
          load-caos-hello-worker = {
            type = "app";
            program = "${loadHelloWorker}/bin/load-caos-hello-worker";
          };
        };

        checks = {
          inherit client object-server compute-server;

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
