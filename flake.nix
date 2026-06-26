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

        # The Linux system whose binaries the Docker images carry. We build for
        # the host's architecture (no arch-cross), so on Linux this is just the
        # host; on macOS it's the matching Linux system, whose general-purpose
        # packages (git, tar, the docker client in the server image) are
        # substituted prebuilt from the binary cache — no local Linux build, no VM.
        linuxSystem = if pkgs.stdenv.hostPlatform.isAarch64 then "aarch64-linux" else "x86_64-linux";
        linuxPkgs = import nixpkgs { system = linuxSystem; };

        # Toolchain is pinned via ./rust-toolchain.toml + the flake.lock'd
        # rust-overlay revision, so every build uses the same compiler.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        src = craneLib.cleanCargoSource ./.;

        # Build for musl so the binary is fully static (crt-static is on by
        # default for musl targets) — its runtime closure is just itself.
        # Target the build host's architecture, no arch-cross: aarch64 on Apple
        # Silicon / aarch64 Linux, x86_64 otherwise. rust-toolchain.toml carries
        # the std for both musl targets so either resolves.
        muslTarget =
          if pkgs.stdenv.hostPlatform.isAarch64 then
            "aarch64-unknown-linux-musl"
          else
            "x86_64-unknown-linux-musl";
        muslEnvTarget = pkgs.lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] muslTarget);

        # On macOS the default linker is Apple ld, which can't link Linux ELF (it
        # rejects GNU flags like --as-needed). rust-lld ships inside the toolchain
        # and links musl ELF cross-platform, so we need no C cross-toolchain.
        # Linux hosts link musl with their native toolchain, so this override is
        # Darwin-only — keeping Linux/CI builds byte-identical.
        muslCrossLinker = pkgs.writeShellScript "caos-rust-lld" ''
          sysroot="$(${rustToolchain}/bin/rustc --print sysroot)"
          exec "$(echo "$sysroot"/lib/rustlib/*/bin/rust-lld)" "$@"
        '';
        crossLinkerEnv = pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin {
          "CARGO_TARGET_${muslEnvTarget}_LINKER" = "${muslCrossLinker}";
          "CARGO_TARGET_${muslEnvTarget}_RUSTFLAGS" = "-Clinker-flavor=ld.lld -Clink-self-contained=yes";
        };

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
          # need a musl cross-toolchain to stay static. (the server's gix
          # uses default-features = false, so it stays pure-Rust / static.)
          # buildInputs = [ ];
          # nativeBuildInputs = [ ];
        }
        // crossLinkerEnv;

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

        # One crate, two binaries: `caos` (worker-side, baked into images) and
        # `caos-cli` (user-facing). crateBin builds the package, so both land in
        # the output's /bin; consumers pick the one they need.
        caos = crateBin "caos";
        server = crateBin "server";
        worker-hello = crateBin "worker-hello";
        worker-fold = crateBin "worker-fold";
        worker-file-count = crateBin "worker-file-count";
        worker-deep-deps = crateBin "worker-deep-deps";
        worker-rustc = crateBin "worker-rustc";

        # Minimal images: each contains *only* its static binary — no shell, no
        # libc, no /nix/store. Crates are unprefixed (caos, server) but
        # the published image names carry a `caos-` prefix.
        # The images are Linux, but build on macOS too (no VM): the Rust binaries
        # cross-compile for the host arch via rust-lld (see muslCrossLinker), and
        # the server image's tools come from linuxPkgs — substituted prebuilt from
        # the binary cache.

        # Worker images carry the `caos` binary (the worker-side client) at
        # `/bin/caos`. The `/cas` directory is *not* baked in — `caos entrypoint`
        # creates it at runtime (so a mounted, empty /cas volume works too).
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
          cp ${caos}/bin/caos bin/caos
          chmod 4755 bin/caos
          mkdir -p tmp
          chmod 1777 tmp
        '';
        # The container runs `caos entrypoint`: set up /cas, run /worker, then
        # print the hash of /cas/out. The server URL a worker needs is injected at
        # runtime by the server for the containers it spawns — so none are baked
        # into the images.
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

        # A "bash" worker: it runs a shell script you hand it. `caos entrypoint`
        # sets up /cas, materializes the run's args under /cas/args (one per
        # --name=value / --name:@=path), and runs us as /worker; we fetch the
        # `script` arg (a text file) and execute it with bash. The script does its
        # work with the in-sandbox `caos` (get/put/run/curry/import-image), reads
        # any other args from /cas/args/<name>, and leaves its result at /cas/out
        # — so orchestration that used to run host-side now runs *inside* a worker,
        # with a real /cas. Curry it (binding the script, or other args) and run it
        # like any other image. Unlike the minimal Rust workers it carries a shell
        # and coreutils, so it's not minimal — but it's a real, memoized worker.
        workerBashScript = pkgs.writeTextFile {
          name = "caos-worker-bash-script";
          executable = true;
          destination = "/worker";
          text = ''
            #!/bin/bash
            # caos entrypoint runs us as /worker, with /cas set up and the args
            # materialized under /cas/args. Fetch the script and run it; on exit
            # caos reads the hash of /cas/out. If the script left no result there,
            # store an empty blob so there's something to read.
            set -euo pipefail
            caos get /cas/args/script
            bash /cas/args/script
            if [ ! -e /cas/out ]; then
              : > /tmp/caos-empty-out
              caos put /tmp/caos-empty-out /cas/out
            fi
          '';
        };
        workerBashContents = [
          workerBaseRoot
          workerBashScript
          pkgs.bash
          pkgs.coreutils
          pkgs.diffutils
          pkgs.gnugrep
          pkgs.findutils
        ];
        workerBashConfig = {
          Entrypoint = [
            "/bin/caos"
            "entrypoint"
          ];
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
        # /cas/out. The server runs it via `caos entrypoint`, which
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
        # is left at /cas/out. Unlike the other workers it drives the server via
        # `caos run` — both to apply pre/post and to recurse — so it relies on
        # CAOS_SERVER_URL (injected by the server) and reaches its own image, for
        # the recursive call, as the built-in /cas/std/fold.
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
          Env = [ "PATH=/bin" ];
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
        # touches the server (no `caos run`); the server injects
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
        # args) and is caught by the server's run-cycle detection.
        #
        # Like fold it drives the server via `caos run`, so it relies on
        # CAOS_SERVER_URL (injected) and reaches its own image (to curry
        # resolve/finish) and fold's as the built-ins /cas/std/deep-deps and
        # /cas/std/fold.
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
          Env = [ "PATH=/bin" ];
        };
        workerDeepDepsImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-deep-deps";
          tag = "latest";
          contents = workerDeepDepsContents;
          config = workerDeepDepsConfig;
          fakeRootCommands = installWorkerFiles;
        };

        # A "rustc" worker: it *builds other workers*. Given a Rust source file as
        # `--src` and a worker-base git-docker image as `--base` (curried in), it
        # compiles the source (static, musl, linking the vendored worker-common)
        # and emits a new worker image at /cas/out — the base's layers plus one
        # carrying the compiled /worker. So this image is far from minimal: it
        # bakes a whole Rust + C toolchain, merged with the worker root via
        # buildEnv so cargo/rustc/cc all land on PATH=/bin. The worker-common
        # source is vendored at /vendor/worker-common for user code to depend on.
        # This is the `worker-rustc` crate at /worker.
        workerRustcRoot = workerRoot "worker-rustc" worker-rustc;
        # The worker-common crate source, for the in-image `cargo build` to link
        # user code against (it has no deps, so no registry/network is needed).
        workerCommonVendor = pkgs.runCommand "caos-worker-common-vendor" { } ''
          mkdir -p $out/vendor
          cp -r ${./crates/worker-common} $out/vendor/worker-common
        '';
        # One merged root tree: the worker bits plus the build toolchain, so a
        # single PATH=/bin reaches caos, /worker, cargo, rustc, and cc.
        workerRustcRootEnv = pkgs.buildEnv {
          name = "caos-worker-rustc-root";
          paths = [
            workerBaseRoot
            workerRustcRoot
            workerCommonVendor
            rustToolchain
            pkgs.stdenv.cc # cc/gcc + binutils, the linker rustc drives for musl
            pkgs.coreutils
            pkgs.bash # cc-wrapper and cargo shell out to these
          ];
        };
        workerRustcContents = [ workerRustcRootEnv ];
        workerRustcConfig = {
          Entrypoint = [
            "/bin/caos"
            "entrypoint"
          ];
          Env = [
            "PATH=/bin"
            # cargo writes here; /tmp is the only world-writable dir for the worker.
            "CARGO_HOME=/tmp/cargo"
          ];
        };
        workerRustcImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-rustc";
          tag = "latest";
          contents = workerRustcContents;
          config = workerRustcConfig;
          fakeRootCommands = installWorkerFiles;
        };

        # The caos server: storage *and* compute in one process (it serves
        # /object from a git repo and /run by spawning worker containers). It runs
        # those containers by shelling out to the `docker` CLI, so — unlike the
        # minimal images — it bundles the docker client and expects the host's
        # docker socket bind-mounted at /var/run/docker.sock, and it shells out to
        # GNU `tar` to build layer tarballs when converting a git image. It also
        # needs the git object database bind-mounted at /git (override with
        # CAOS_GIT_DIR):
        #   docker run --rm --network caos-net -p 9090:80 \
        #     -v /var/run/docker.sock:/var/run/docker.sock -v /repo/.git:/git \
        #     caos-server
        serverContents = [
          server
          # General-purpose Linux tools the server shells out to. Pulled from a
          # Linux nixpkgs (see linuxPkgs) so they're real ELF binaries — on macOS
          # they're substituted prebuilt from the cache rather than built.
          linuxPkgs.docker-client
          linuxPkgs.gnutar
          # `git http-backend` (and the `git` it dispatches): the smart-HTTP
          # transport the caos client uses as its `caos` remote.
          linuxPkgs.git
        ];
        serverConfig = {
          Cmd = [ "/bin/server" ];
          Env = [ "PATH=/bin" ];
          ExposedPorts = {
            "80/tcp" = { };
          };
        };
        serverImage = pkgs.dockerTools.buildImage {
          name = "caos-server";
          tag = "latest";
          copyToRoot = serverContents;
          config = serverConfig;
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
        loadWorkerBash = loadImage {
          name = "caos-worker-bash";
          contents = workerBashContents;
          config = workerBashConfig;
          fakeRootCommands = installWorkerFiles;
        };
        loadServer = loadImage {
          name = "caos-server";
          contents = serverContents;
          config = serverConfig;
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
        loadWorkerRustc = loadImage {
          name = "caos-worker-rustc";
          contents = workerRustcContents;
          config = workerRustcConfig;
          fakeRootCommands = installWorkerFiles;
        };

        # ---- Cross-tree consumption: caos-cli, the stack, the stdlib ----
        # These let another tree (one that has caos as a flake input) get the
        # user-facing CLI on its PATH, bring the dev stack up, and publish the
        # builtin worker library — without the caos source tree or `tilt`.

        # Just the user-facing CLI (a consumer wants only `caos-cli`, not the
        # worker-side `caos`, in its devShell) — and it runs on the *host*. On
        # Linux the musl `caos-cli` already runs on the host, so copy it straight
        # out of the `caos` package; on macOS that's a Linux binary, so build a
        # native `caos-cli` for the host instead.
        nativeArgs = {
          inherit src;
          strictDeps = true;
          pname = "caos-cli";
          version = "0.1.0";
        };
        caos-cli =
          if pkgs.stdenv.hostPlatform.isLinux then
            pkgs.runCommand "caos-cli" { } ''
              mkdir -p $out/bin
              cp ${caos}/bin/caos-cli $out/bin/caos-cli
            ''
          else
            craneLib.buildPackage (
              nativeArgs
              // {
                cargoArtifacts = craneLib.buildDepsOnly nativeArgs;
                cargoExtraArgs = "--package caos --bin caos-cli";
                doCheck = false;
              }
            );

        # All the host-facing caos commands in one package, so a consumer lists
        # *this* in its devShell and gets `caos-cli` and `caosd` on PATH together
        # (like `pkgs.typescript` giving you tsc + tsserver) — no enumerating the
        # individual tools. symlinkJoin merges their /bin into one output.
        caos-tools = pkgs.symlinkJoin {
          name = "caos-tools";
          paths = [
            caos-cli
            caosd
          ];
        };

        # The single source of truth for "what's in std" — the builtin worker
        # images by their library name (clients reach them as `/cas/std/<name>`).
        builtinImages = {
          base = workerBaseImage;
          bash = workerBashImage;
          hello = workerHelloImage;
          fold = workerFoldImage;
          file-count = workerFileCountImage;
          deep-deps = workerDeepDepsImage;
          rustc = workerRustcImage;
        };

        # One import per builtin, baked at eval time (image store paths inlined —
        # no runtime `nix build`). Each writes the image's git objects into the
        # bare repo and appends its `git mktree` line (name -> image tree).
        stdImportLines = pkgs.lib.concatStringsSep "\n" (pkgs.lib.mapAttrsToList
          (name: img: ''
            echo "  importing builtin: ${name}" >&2
            hash="$(cd "$BARE" && caos-cli import-image "${img}")"
            printf '040000 tree %s\t%s\n' "$hash" "${name}" >> "$scratch/tree.txt"'')
          builtinImages);

        # The dev stack as docker compose: redis + registry + the caos server,
        # mirroring the Tiltfile's wiring. The network and container names are
        # *pinned* (not compose's project-prefixed defaults) so the worker
        # containers the server spawns over the docker socket — which it attaches
        # to CAOS_DOCKER_NETWORK by this literal name — land on caos-net and can
        # reach caos-registry. The server image is loaded from the Nix store
        # (pull_policy: never), and CAOS_DATA (absolute) holds its bare repo.
        composeFile = pkgs.writeText "docker-compose.yml" ''
          # caos dev stack — generated by the caos flake. Driven by `caosd`,
          # which sets CAOS_DATA, loads the server image, and inits the repo +
          # stdlib before running this file. A bare `docker compose up` works
          # only once those prerequisites are in place.
          name: caos
          networks:
            caos-net:
              name: caos-net
          services:
            caos-redis:
              image: redis:7
              container_name: caos-redis
              networks: [caos-net]
              ports: ["6379:6379"]
            caos-registry:
              image: registry:2
              container_name: caos-registry
              networks: [caos-net]
              ports: ["5000:5000"]
            caos-server:
              image: caos-server:latest
              container_name: caos-server
              pull_policy: never
              networks: [caos-net]
              ports: ["9090:80"]
              environment:
                CAOS_DOCKER_NETWORK: caos-net
              volumes:
                - /var/run/docker.sock:/var/run/docker.sock
                - "''${CAOS_DATA:?set CAOS_DATA to an absolute data dir}/server-repo.git:/git"
              depends_on: [caos-redis, caos-registry]
        '';

        # Publish the builtin library into the server's *own* bare repo. Not a
        # public output — caosd runs it on every startup. Pure
        # local git plumbing — no server round-trip, no `git push`: import each
        # image's objects straight into the repo, then point refs/caos/std at the
        # assembled {name: image} tree. Safe to run against a live server: objects
        # are only added, and the ref swaps atomically (update-ref renames), so an
        # in-flight client fetch sees either the whole old or the whole new lib.
        set-stdlib = pkgs.writeShellApplication {
          name = "set-stdlib";
          runtimeInputs = [ pkgs.git pkgs.coreutils caos ];
          text = ''
            : "''${CAOS_DATA:=$PWD/.caos-data}"
            CAOS_DATA="$(readlink -m "$CAOS_DATA")"
            BARE="$CAOS_DATA/server-repo.git"

            # The server's bare repo + the smart-HTTP transport bits clients use
            # (push objects up, fetch results by bare hash back). Idempotent, so
            # re-running — or running after caosd already made it — is fine.
            git init -q --bare "$BARE"
            git -C "$BARE" config http.receivepack true
            git -C "$BARE" config uploadpack.allowAnySHA1InWant true

            # caos-cli's git transport discovers the repo from the cwd, so the
            # imports below `cd "$BARE"`; scratch just collects the mktree lines.
            scratch="$(mktemp -d)"
            trap 'rm -rf "$scratch"' EXIT
            : > "$scratch/tree.txt"

            ${stdImportLines}

            tree="$(git -C "$BARE" mktree < "$scratch/tree.txt")"
            git -C "$BARE" update-ref refs/caos/std "$tree"
            echo "refs/caos/std -> $tree (in $BARE)" >&2
          '';
        };

        # Bring the stack up in the foreground (Ctrl-C tears it down). Builds (or
        # cache-hits) and loads the server image, inits the bare repo, publishes
        # the stdlib into it, then `docker compose up`. Uses the host's docker /
        # `docker compose` — they must target the same daemon the server's socket
        # mount points at. CAOS_DATA (made absolute) persists the repo across runs.
        caosd = pkgs.writeShellApplication {
          name = "caosd";
          runtimeInputs = [ pkgs.coreutils set-stdlib ];
          text = ''
            : "''${CAOS_DATA:=$PWD/.caos-data}"
            CAOS_DATA="$(readlink -m "$CAOS_DATA")"
            export CAOS_DATA
            mkdir -p "$CAOS_DATA"

            echo "==> loading caos-server image into docker" >&2
            docker load -i ${serverImage}

            echo "==> publishing stdlib into $CAOS_DATA/server-repo.git" >&2
            set-stdlib

            echo "==> starting stack (redis, registry, server) — Ctrl-C to stop" >&2
            trap 'docker compose -f ${composeFile} down' EXIT
            docker compose -f ${composeFile} up
          '';
        };
      in
      {
        packages = {
          default = caos;
          inherit caos server caos-cli caosd caos-tools;

          # The generated compose file, for driving the stack by hand
          # (`docker compose -f $(nix build --print-out-paths .#docker-compose)
          # up`). `caosd` is the batteries-included way.
          docker-compose = composeFile;

          # Image tarballs (build with `nix build`, then `docker load < result`).
          caos-worker-base-docker = workerBaseImage;
          caos-worker-bash-docker = workerBashImage;
          caos-server-docker = serverImage;
          caos-worker-hello-docker = workerHelloImage;
          caos-worker-fold-docker = workerFoldImage;
          caos-worker-file-count-docker = workerFileCountImage;
          caos-worker-deep-deps-docker = workerDeepDepsImage;
          caos-worker-rustc-docker = workerRustcImage;
        };

        apps = {
          # Bring the whole stack up in the foreground (Ctrl-C tears it down).
          caosd = {
            type = "app";
            program = "${caosd}/bin/caosd";
          };

          # Build the image and load it into the local docker daemon in one go.
          load-caos-worker-base = {
            type = "app";
            program = "${loadWorkerBase}/bin/load-caos-worker-base";
          };
          load-caos-worker-bash = {
            type = "app";
            program = "${loadWorkerBash}/bin/load-caos-worker-bash";
          };
          load-caos-server = {
            type = "app";
            program = "${loadServer}/bin/load-caos-server";
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
          load-caos-worker-rustc = {
            type = "app";
            program = "${loadWorkerRustc}/bin/load-caos-worker-rustc";
          };
        };

        checks = {
          inherit caos server;

          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          fmt = craneLib.cargoFmt { inherit src; };
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # cargoTest builds *and runs* the test binaries, which are musl/Linux —
          # they can't execute on a macOS host, so this check is Linux-only.
          # On macOS, run tests in the dev shell with `cargo test` (native target).
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
