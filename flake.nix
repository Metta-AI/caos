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
        worker-hello = crateBin "worker-hello";
        worker-fold = crateBin "worker-fold";
        worker-file-count = crateBin "worker-file-count";
        worker-deep-deps = crateBin "worker-deep-deps";

        # Minimal images: each contains *only* its static binary — no shell, no
        # libc, no /nix/store. Crates are unprefixed (client, object-server) but
        # the published image names carry a `caos-` prefix.
        # NOTE: Docker images are Linux-only; build these on Linux (or via a
        # remote/linux builder on macOS).

        # The client crate's binary is `client` everywhere except inside its
        # image, where it's exposed as `/bin/caos`. The `/cas` directory is *not*
        # baked in — `caos entrypoint` creates it at runtime (so a mounted, empty
        # /cas volume works too).
        #
        # This store path holds only what needs *no* special permissions: the user
        # database. `caos` itself can't live here — it must be setuid-root (so a
        # non-root worker can mutate the root-owned /cas through it, and only
        # through it), and Nix strips the setuid bit when it seals a store path.
        # So caos (and a writable /tmp) are installed per-image by
        # `installWorkerFiles` below, which runs while the image layer is built.
        # A worker image root: a single static worker binary placed at /worker.
        # Each Rust worker crate's binary is named after its package, so pass that
        # name and the built crate. The result is combined with workerBaseRoot
        # (the user database) and the setuid caos installed by installWorkerFiles —
        # no shell or coreutils, since the worker itself does all the file work.
        workerRoot = binName: drv: pkgs.runCommand "caos-${binName}-root" { } ''
          mkdir -p $out
          cp ${drv}/bin/${binName} $out/worker
        '';

        workerBaseRoot = pkgs.runCommand "caos-worker-base-root" { } ''
          mkdir -p $out/etc
          printf 'root:x:0:0:root:/root:/sbin/nologin\n' > $out/etc/passwd
          printf 'worker:x:1000:1000:caos worker:/tmp:/sbin/nologin\n' >> $out/etc/passwd
          printf 'root:x:0:\nworker:x:1000:\n' > $out/etc/group
        '';

        # Commands run while assembling a worker image's layer (under fakeroot, so
        # everything is recorded as root-owned): install caos as a setuid-root
        # binary and create the world-writable /tmp the unprivileged worker needs
        # (it can't create one under the root-owned /). `bin` is always a real
        # directory here (the base image makes it; the others get it from
        # bash/coreutils), so the copy lands as a real file the chmod can mark.
        installWorkerFiles = ''
          mkdir -p bin
          cp ${client}/bin/client bin/caos
          chmod 4755 bin/caos
          mkdir -p tmp
          chmod 1777 tmp
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
        # Layered (not buildImage) so we can use fakeRootCommands to install the
        # setuid-root caos — see installWorkerFiles.
        workerBaseImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-base";
          tag = "latest";
          contents = [ workerBaseRoot ];
          config = workerBaseConfig;
          fakeRootCommands = installWorkerFiles;
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
        workerBashImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-bash";
          tag = "latest";
          contents = workerBashContents;
          config = workerBashConfig;
          fakeRootCommands = installWorkerFiles;
        };

        # A real, runnable worker image, with a /worker that reads its inputs from
        # /cas/args (one entry per `--name=value` arg `caos run` passed), assembles
        # them into a result tree along with a small receipt, and stores that at
        # /cas/out. The compute server runs it via `caos entrypoint`, which
        # populates /cas/args and runs /worker. This is the `worker-hello` crate, a
        # static binary at /worker — so the image needs no shell or coreutils.
        workerHelloRoot = workerRoot "worker-hello" worker-hello;
        workerHelloContents = [
          workerBaseRoot
          workerHelloRoot
        ];
        workerHelloConfig = {
          Entrypoint = [
            "/bin/caos"
            "entrypoint"
          ];
          Env = [ "PATH=/bin" ];
        };
        workerHelloImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-hello";
          tag = "latest";
          contents = workerHelloContents;
          config = workerHelloConfig;
          fakeRootCommands = installWorkerFiles;
        };

        # A recursive "fold" worker — a catamorphism over a CAS tree. Args:
        #   pre  — (optional) image applied to `in` to produce the tree of
        #          children to fold; omitted means the structural default (a
        #          tree's own children; a file is a childless leaf).
        #   post — image applied to `in` plus `--children` (the folded child
        #          results, by name) to combine them into this node's result.
        #   in   — the file or tree to fold over, a CAS path.
        # So fold decides a node's children (via pre, or structurally), folds
        # each child with itself (the same pre/post), then combines them with
        # post. pre/post are image refs — often curried with the context they
        # need — and the applied images take their input as `--in`; the result
        # is left at /cas/out. Unlike the other workers it drives the compute
        # server via `caos run` — both to apply pre/post and to recurse — so it
        # relies on CAOS_COMPUTE_SERVER_URL (injected by the compute server) and
        # learns its own image name, for the recursive call, from CAOS_FOLD_IMAGE.
        # This is the `worker-fold` crate, a static binary at /worker — so the
        # image needs no shell or coreutils.
        workerFoldRoot = workerRoot "worker-fold" worker-fold;
        workerFoldContents = [
          workerBaseRoot
          workerFoldRoot
        ];
        workerFoldConfig = {
          Entrypoint = [
            "/bin/caos"
            "entrypoint"
          ];
          Env = [
            "PATH=/bin"
            # caos run defaults to git images, so name the docker image explicitly.
            "CAOS_FOLD_IMAGE=docker://caos-worker-fold:latest"
          ];
        };
        workerFoldImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-fold";
          tag = "latest";
          contents = workerFoldContents;
          config = workerFoldConfig;
          fakeRootCommands = installWorkerFiles;
        };

        # A "file-count" worker: a leaf algebra meant to drive fold as its
        # `post`. It gets the node as `--in` and the folded child results as
        # `--children`. A file (a leaf) counts as 1; otherwise it returns the sum
        # of its child counts (each `--children` entry holds a number). The
        # result, a blob holding the count, is left at /cas/out. So
        # `fold --post=file-count` over a tree totals its leaf files. It only
        # touches the object server (no `caos run`); the compute server injects
        # that URL at runtime. This is the `worker-file-count` crate, a static
        # binary at /worker — so the image needs no shell or coreutils.
        workerFileCountRoot = workerRoot "worker-file-count" worker-file-count;
        workerFileCountContents = [
          workerBaseRoot
          workerFileCountRoot
        ];
        workerFileCountConfig = {
          Entrypoint = [
            "/bin/caos"
            "entrypoint"
          ];
          Env = [ "PATH=/bin" ];
        };
        workerFileCountImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-file-count";
          tag = "latest";
          contents = workerFileCountContents;
          config = workerFileCountConfig;
          fakeRootCommands = installWorkerFiles;
        };

        # A "deep-deps" worker: turns a flat, name-keyed package map into a DAG of
        # deepened nodes. The input `packages` tree holds one subtree per
        # package, each with a `DEPS` blob (dependency names, one per line). The
        # output mirrors it, but each node carries a `DEEP-DEPS` subtree of its
        # recursively-deepened direct deps (which themselves carry DEEP-DEPS).
        #
        # It's written as a fold (caos-worker-fold) over the dependency graph,
        # with this same image — curried so fold sees plain images — supplying
        # the fold's two functions. `--mode` is optional; omitting it is the
        # simple public API:
        #   (no mode) — deepen one package (`--name`): run fold over it.
        #   all       — top-level convenience: deepen every package.
        # The internal modes, reached only via curry by the driver:
        #   resolve — fold's `pre`, curried with `--packages` (the whole map):
        #             given a package as `--in`, resolve its `DEPS` names to the
        #             dep subtrees to recurse into.
        #   finish  — fold's `post`: given a package as `--in` and its deepened
        #             deps as `--children`, build the node (the package's files,
        #             minus DEPS, plus a DEEP-DEPS of the children).
        # Incrementality comes entirely from CAOS call memoization. The driver
        # and resolve carry the whole map, so they re-run on any edit — cheap
        # orchestration. But finish (curried with nothing) is keyed only on a
        # package and its deepened subgraph, so real recompute is O(changed
        # package + its dependents). A cycle re-enters the same fold (image,
        # args) and is caught by the compute server's run-cycle detection.
        #
        # Like fold it drives the compute server via `caos run`, so it relies on
        # CAOS_COMPUTE_SERVER_URL (injected) and learns its own image (to curry
        # resolve/finish) from CAOS_DEEP_DEPS_IMAGE and fold's from
        # CAOS_FOLD_IMAGE.
        #
        # This worker is the `worker-deep-deps` crate, a static binary placed at
        # /worker — so, like the other Rust workers, its image needs no shell or
        # coreutils, just caos (installed setuid by installWorkerFiles).
        workerDeepDepsRoot = workerRoot "worker-deep-deps" worker-deep-deps;
        workerDeepDepsContents = [
          workerBaseRoot
          workerDeepDepsRoot
        ];
        workerDeepDepsConfig = {
          Entrypoint = [
            "/bin/caos"
            "entrypoint"
          ];
          Env = [
            "PATH=/bin"
            # caos run defaults to git images, so name the docker images explicitly.
            "CAOS_DEEP_DEPS_IMAGE=docker://caos-worker-deep-deps:latest"
            "CAOS_FOLD_IMAGE=docker://caos-worker-fold:latest"
          ];
        };
        workerDeepDepsImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-deep-deps";
          tag = "latest";
          contents = workerDeepDepsContents;
          config = workerDeepDepsConfig;
          fakeRootCommands = installWorkerFiles;
        };

        # compute-server runs worker containers by shelling out to the `docker`
        # CLI, so — unlike the minimal images — it bundles the docker client and
        # expects the host's docker socket bind-mounted at /var/run/docker.sock.
        # It also shells out to GNU `tar` to build layer tarballs when converting
        # a git image, so it bundles that too:
        #   docker run --rm --network caos-net -p 9090:80 \
        #     -v /var/run/docker.sock:/var/run/docker.sock caos-compute-server
        computeServerContents = [
          compute-server
          pkgs.docker-client
          pkgs.gnutar
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
          { name, contents, config ? { }, fakeRootCommands ? "" }:
          let
            stream = pkgs.dockerTools.streamLayeredImage {
              inherit name config contents fakeRootCommands;
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
          fakeRootCommands = installWorkerFiles;
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
          fakeRootCommands = installWorkerFiles;
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
          fakeRootCommands = installWorkerFiles;
        };
        loadWorkerFold = loadImage {
          name = "caos-worker-fold";
          contents = workerFoldContents;
          config = workerFoldConfig;
          fakeRootCommands = installWorkerFiles;
        };
        loadWorkerFileCount = loadImage {
          name = "caos-worker-file-count";
          contents = workerFileCountContents;
          config = workerFileCountConfig;
          fakeRootCommands = installWorkerFiles;
        };
        loadWorkerDeepDeps = loadImage {
          name = "caos-worker-deep-deps";
          contents = workerDeepDepsContents;
          config = workerDeepDepsConfig;
          fakeRootCommands = installWorkerFiles;
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
          caos-worker-deep-deps-docker = workerDeepDepsImage;
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
          load-caos-worker-deep-deps = {
            type = "app";
            program = "${loadWorkerDeepDeps}/bin/load-caos-worker-deep-deps";
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
