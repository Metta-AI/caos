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
        workerBaseRoot = pkgs.runCommand "caos-worker-base-root" { } ''
          mkdir -p $out/bin
          cp ${client}/bin/client $out/bin/caos
        '';
        # The container runs `caos entrypoint`: set up /cas, run /worker, then
        # print the hash of /cas/out. The object- and compute-server URLs a worker
        # needs are injected at runtime — by the compute server for the containers
        # it spawns, and by ./run-worker-bash.sh for the debug shell — so none are
        # baked into the images.
        workerBaseConfig = {
          Entrypoint = [
            "/bin/caos"
            "entrypoint"
          ];
        };
        workerBaseImage = pkgs.dockerTools.buildImage {
          name = "caos-worker-base";
          tag = "latest";
          copyToRoot = [ workerBaseRoot ];
          config = workerBaseConfig;
        };

        # Run with the git repo bind-mounted at /git, e.g.
        #   docker run --rm -p 8080:80 -v /path/to/repo:/git caos-object-server
        objectServerImage = pkgs.dockerTools.buildImage {
          name = "caos-object-server";
          tag = "latest";
          copyToRoot = [ object-server ];
          config = {
            Cmd = [ "/bin/object-server" ];
            ExposedPorts = {
              "80/tcp" = { };
            };
          };
        };

        # A testing image: caos-worker-base plus an ordinary interactive shell
        # (bash + coreutils + curl) so you can poke at /cas and the object server
        # by hand. Not minimal — for debugging, not production. Like the other
        # workers it extends caos-worker-base and runs `caos entrypoint`, which
        # sets up /cas and runs /worker; here /worker drops you into an interactive
        # shell. Run it with ./run-worker-bash.sh, which wires up the daemon URLs.
        workerBashScript = pkgs.writeTextFile {
          name = "caos-worker-bash-script";
          executable = true;
          destination = "/worker";
          text = ''
            #!/bin/bash
            # Interactive debugging shell. `caos entrypoint` runs us as /worker
            # (with /cas already set up) and, on exit, reads the hash of /cas/out.
            # Drop into a shell, then — if you didn't leave a result there — store
            # an empty blob at /cas/out so exiting doesn't error.
            bash
            if [ ! -e /cas/out ]; then
              mkdir -p /tmp
              touch /tmp/caos-empty-out
              caos put /tmp/caos-empty-out /cas/out
            fi
            exit 0
          '';
        };
        workerBashContents = [
          workerBaseRoot
          workerBashScript
          pkgs.bashInteractive
          pkgs.coreutils
          pkgs.curl
        ];
        workerBashConfig = {
          Entrypoint = [
            "/bin/caos"
            "entrypoint"
          ];
          # Daemon URLs are injected at runtime by ./run-worker-bash.sh.
          Env = [ "PATH=/bin" ];
        };
        workerBashImage = pkgs.dockerTools.buildImage {
          name = "caos-worker-bash";
          tag = "latest";
          copyToRoot = workerBashContents;
          config = workerBashConfig;
        };

        # A real, runnable worker image: caos + bash + coreutils, with a /worker
        # that reads its inputs from /cas/args (one file per `--name=value` arg
        # `caos run` passed), copies them into a result directory along with a
        # small receipt, and stores that at /cas/out. The compute server runs it
        # via `caos entrypoint`, which populates /cas/args and runs /worker.
        workerHelloScript = pkgs.writeTextFile {
          name = "caos-worker-hello-script";
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
        workerHelloContents = [
          workerBaseRoot
          workerHelloScript
          pkgs.bashInteractive
          pkgs.coreutils
        ];
        workerHelloConfig = {
          Entrypoint = [
            "/bin/caos"
            "entrypoint"
          ];
          Env = [ "PATH=/bin" ];
        };
        workerHelloImage = pkgs.dockerTools.buildImage {
          name = "caos-worker-hello";
          tag = "latest";
          copyToRoot = workerHelloContents;
          config = workerHelloConfig;
        };

        # A recursive "fold" worker — a catamorphism over a CAS tree. Two args:
        #   func — the worker image to apply (the "algebra"), a literal value
        #   in   — the file or tree to fold over, a CAS path
        # Given a file it runs `func` on it. Given a tree it folds each child
        # with itself (the same func), assembles the results into a tree with
        # the original child names, then runs `func` on that tree. Like every
        # worker, the applied image takes its single input as `--in`; the result
        # is left at /cas/out. Unlike the other workers it drives the compute
        # server via `caos run` — both to apply `func` and to recurse — so it
        # relies on CAOS_COMPUTE_SERVER_URL (injected by the compute server) and
        # learns its own image name, for the recursive call, from CAOS_FOLD_IMAGE.
        workerFoldScript = pkgs.writeTextFile {
          name = "caos-worker-fold-script";
          executable = true;
          destination = "/worker";
          text = ''
            #!/bin/bash
            set -euo pipefail

            fold_image=''${CAOS_FOLD_IMAGE:-caos-worker-fold:latest}

            # The function to apply is a blob arg: expand the placeholder and read it.
            caos get /cas/args/func
            func=$(cat /cas/args/func)

            if [ -d /cas/args/in ]; then
              echo "fold: input is a tree; folding its children with $func" >&2

              # Expand the tree one level: a placeholder per child.
              caos get /cas/args/in

              work=/tmp/folded
              rm -rf "$work"
              mkdir -p "$work"

              i=0
              for child in /cas/args/in/*; do
                [ -e "$child" ] || continue   # empty tree: the glob stays literal
                name=$(basename "$child")
                # Fold this child with the same function; its result lands at /cas/c<i>.
                caos run "$fold_image" "/cas/c$i" -- \
                  --func="$func" --in="$child"
                # Symlink into the CAS so `caos put` reuses the result's recorded
                # hash (no content re-read) under the child's original name.
                ln -s "/cas/c$i" "$work/$name"
                echo "  folded $name -> /cas/c$i" >&2
                i=$((i + 1))
              done

              # Assemble the folded children into a tree, then apply the function.
              caos put "$work" /cas/folded
              caos run "$func" /cas/out -- --in=/cas/folded
            else
              echo "fold: input is a file; applying $func" >&2
              caos run "$func" /cas/out -- --in=/cas/args/in
            fi
          '';
        };
        workerFoldContents = [
          workerBaseRoot
          workerFoldScript
          pkgs.bashInteractive
          pkgs.coreutils
        ];
        workerFoldConfig = {
          Entrypoint = [
            "/bin/caos"
            "entrypoint"
          ];
          Env = [
            "PATH=/bin"
            "CAOS_FOLD_IMAGE=caos-worker-fold:latest"
          ];
        };
        workerFoldImage = pkgs.dockerTools.buildImage {
          name = "caos-worker-fold";
          tag = "latest";
          copyToRoot = workerFoldContents;
          config = workerFoldConfig;
        };

        # A "file-count" worker: a leaf algebra meant to be driven by the fold
        # worker. Its single input arrives as `--in`. A file counts as 1; a
        # directory (assumed to hold only files, each containing a number — e.g.
        # the per-child counts fold assembles) returns their sum. The result, a
        # blob holding the count, is left at /cas/out. So folding a tree with
        # this image totals the leaf files. It only touches the object server
        # (no `caos run`); the compute server injects that URL at runtime.
        workerFileCountScript = pkgs.writeTextFile {
          name = "caos-worker-file-count-script";
          executable = true;
          destination = "/worker";
          text = ''
            #!/bin/bash
            set -euo pipefail

            if [ -d /cas/args/in ]; then
              echo "file-count: summing child counts" >&2
              # Expand the directory one level: a placeholder per child file.
              caos get /cas/args/in
              total=0
              for child in /cas/args/in/*; do
                [ -e "$child" ] || continue   # empty directory: glob stays literal
                caos get "$child"             # expand the placeholder to its bytes
                total=$((total + $(cat "$child")))
              done
            else
              echo "file-count: a file counts as 1" >&2
              total=1
            fi

            # These minimal images ship no /tmp; create it before writing there.
            mkdir -p /tmp
            out=/tmp/count
            printf '%s\n' "$total" > "$out"
            caos put "$out" /cas/out
          '';
        };
        workerFileCountContents = [
          workerBaseRoot
          workerFileCountScript
          pkgs.bashInteractive
          pkgs.coreutils
        ];
        workerFileCountConfig = {
          Entrypoint = [
            "/bin/caos"
            "entrypoint"
          ];
          Env = [ "PATH=/bin" ];
        };
        workerFileCountImage = pkgs.dockerTools.buildImage {
          name = "caos-worker-file-count";
          tag = "latest";
          copyToRoot = workerFileCountContents;
          config = workerFileCountConfig;
        };

        # compute-server runs worker containers by shelling out to the `docker`
        # CLI, so — unlike the minimal images — it bundles the docker client and
        # expects the host's docker socket bind-mounted at /var/run/docker.sock:
        #   docker run --rm --network caos-net -p 9090:80 \
        #     -v /var/run/docker.sock:/var/run/docker.sock caos-compute-server
        computeServerContents = [
          compute-server
          pkgs.docker-client
        ];
        computeServerConfig = {
          Cmd = [ "/bin/compute-server" ];
          Env = [ "PATH=/bin" ];
          ExposedPorts = {
            "80/tcp" = { };
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

        loadWorkerBase = loadImage {
          name = "caos-worker-base";
          contents = [ workerBaseRoot ];
          config = workerBaseConfig;
        };
        loadObjectServer = loadImage {
          name = "caos-object-server";
          contents = [ object-server ];
          config = {
            Cmd = [ "/bin/object-server" ];
            ExposedPorts = {
              "80/tcp" = { };
            };
          };
        };
        loadWorkerBash = loadImage {
          name = "caos-worker-bash";
          contents = workerBashContents;
          config = workerBashConfig;
        };
        loadComputeServer = loadImage {
          name = "caos-compute-server";
          contents = computeServerContents;
          config = computeServerConfig;
        };
        loadWorkerHello = loadImage {
          name = "caos-worker-hello";
          contents = workerHelloContents;
          config = workerHelloConfig;
        };
        loadWorkerFold = loadImage {
          name = "caos-worker-fold";
          contents = workerFoldContents;
          config = workerFoldConfig;
        };
        loadWorkerFileCount = loadImage {
          name = "caos-worker-file-count";
          contents = workerFileCountContents;
          config = workerFileCountConfig;
        };
      in
      {
        packages = {
          default = client;
          inherit client object-server compute-server;

          # Image tarballs (build with `nix build`, then `docker load < result`).
          caos-worker-base-docker = workerBaseImage;
          caos-object-server-docker = objectServerImage;
          caos-worker-bash-docker = workerBashImage;
          caos-compute-server-docker = computeServerImage;
          caos-worker-hello-docker = workerHelloImage;
          caos-worker-fold-docker = workerFoldImage;
          caos-worker-file-count-docker = workerFileCountImage;
        };

        apps = {
          # Build the image and load it into the local docker daemon in one go.
          load-caos-worker-base = {
            type = "app";
            program = "${loadWorkerBase}/bin/load-caos-worker-base";
          };
          load-caos-object-server = {
            type = "app";
            program = "${loadObjectServer}/bin/load-caos-object-server";
          };
          load-caos-worker-bash = {
            type = "app";
            program = "${loadWorkerBash}/bin/load-caos-worker-bash";
          };
          load-caos-compute-server = {
            type = "app";
            program = "${loadComputeServer}/bin/load-caos-compute-server";
          };
          load-caos-worker-hello = {
            type = "app";
            program = "${loadWorkerHello}/bin/load-caos-worker-hello";
          };
          load-caos-worker-fold = {
            type = "app";
            program = "${loadWorkerFold}/bin/load-caos-worker-fold";
          };
          load-caos-worker-file-count = {
            type = "app";
            program = "${loadWorkerFileCount}/bin/load-caos-worker-file-count";
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
            # `tilt up` builds the images and runs the daemons (see ./Tiltfile).
            pkgs.tilt
          ];
        };
      }
    );
}
