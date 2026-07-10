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
        linuxPkgs = import nixpkgs { system = linuxSystem; inherit overlays; };

        # Toolchain is pinned via ./rust-toolchain.toml + the flake.lock'd
        # rust-overlay revision, so every build uses the same compiler.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        # A Linux copy of the same toolchain, for worker images that bake a Rust
        # compiler into the container (e.g. the rustc builder). On Linux this is
        # the same as rustToolchain; on macOS it's a substituted Linux build.
        linuxRustToolchain = linuxPkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
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
        runnerd = crateBin "runnerd";
        worker-hello = crateBin "worker-hello";
        worker-file-count = crateBin "worker-file-count";
        worker-dirs-only = crateBin "worker-dirs-only";
        worker-deep-deps = crateBin "worker-deep-deps";
        worker-rustc = crateBin "worker-rustc";
        worker-runner = crateBin "worker-runner";

        # Minimal images: each contains *only* its static binary — no shell, no
        # libc, no /nix/store. Crates are unprefixed (caos, server) but
        # the published image names carry a `caos-` prefix.
        # The images are Linux, but build on macOS too (no VM): the Rust binaries
        # cross-compile for the host arch via rust-lld (see muslCrossLinker), and
        # the server image's tools come from linuxPkgs — substituted prebuilt from
        # the binary cache.

        # Worker images carry the `caos` binary (the worker-side client) at
        # `/bin/caos`. The `/cas` directory is *not* baked in — the `caos` runner
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
        # The same, but for images that stack on a stock docker base (e.g. rustc on
        # rust:1-bookworm). There we must NOT create a real /bin or /tmp: on Debian
        # /bin is a symlink to /usr/bin, and a real /bin in our layer would shadow
        # that symlink (hiding cc, sh, …); /tmp already exists. Install caos into
        # /usr/bin (a real dir on the base — our layer MERGES with it, leaving the
        # base's binaries intact) so it's reachable both via PATH and as `/bin/caos`
        # (which the base's /bin -> usr/bin symlink resolves) — the latter matters
        # because runnerd forces `--entrypoint /bin/caos` on every
        # worker, regardless of the image's own Entrypoint.
        installWorkerFilesBaseStacked = ''
          mkdir -p usr/bin
          cp ${caos}/bin/caos usr/bin/caos
          chmod 4755 usr/bin/caos
        '';
        # The container runs `caos runner`: set up /cas, run /worker, post the
        # hash of /cas/out back to the server, then poll for more work. The
        # server URL a worker needs is injected at runtime by runnerd for the
        # containers it spawns — so none are baked into the images. PATH is
        # Debian's default (the base is a stock Debian image; /bin -> /usr/bin,
        # where caos lives).
        workerBaseConfig = {
          Entrypoint = [
            "/bin/caos"
            "runner"
          ];
          Env = [
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
          ];
        };
        # The worker base is a thin delta on a stock glibc base
        # (docker://debian:stable-slim — see build-builtins.sh). It carries only the
        # setuid caos (installed at /usr/bin via installWorkerFilesBaseStacked); the
        # glibc/coreutils come from the Debian base. It exists so workers built from
        # source (worker-rustc, which compiles glibc-dynamic) can stack their
        # /worker on a base that actually provides glibc — and they inherit this
        # setuid caos layer by hash, so the unprivileged builder never has to
        # synthesize a setuid-root binary itself.
        workerBaseImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-base";
          tag = "latest";
          contents = [ ];
          config = workerBaseConfig;
          fakeRootCommands = installWorkerFilesBaseStacked;
        };

        # A self-contained (musl) base layer shared by the non-source-built workers
        # (bash/hello/file-count/deep-deps) via `fromImage` below. Distinct
        # from workerBaseImage (the stock-glibc `base` builtin): these workers are
        # self-contained musl images — they don't stack on a docker:// base — so
        # they need the setuid caos at the real /bin/caos and the user db, exactly
        # the old base. Sharing this one layer means a worker provisioned after any
        # other only uploads its own delta, not the caos binary again (the registry
        # dedups the identical base blob). It is NOT a builtin — just a fromImage.
        workerSharedBaseConfig = {
          Entrypoint = [
            "/bin/caos"
            "runner"
          ];
          Env = [ "PATH=/bin" ];
        };
        workerSharedBaseImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-shared-base";
          tag = "latest";
          contents = [ workerBaseRoot ];
          config = workerSharedBaseConfig;
          fakeRootCommands = installWorkerFiles;
        };

        # A "bash" worker: it runs a shell script you hand it. The `caos` runner
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
            # The caos runner runs us as /worker, with /cas set up and the args
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
        # bash's own files, merged into one root so they land as a single layer
        # atop the shared workerSharedBaseImage (which already carries the setuid
        # caos, /tmp, and the user db). Sharing that base means a worker provisioned
        # after any other only uploads this layer, not the caos binary again. The
        # binaries come from linuxPkgs so they're real Linux ELF even when the flake
        # is evaluated on macOS (see linuxPkgs). The env itself is built with the
        # *host's* buildEnv — it only symlinks store paths, so it needs no Linux
        # builder even though its contents are Linux binaries.
        workerBashRoot = pkgs.buildEnv {
          name = "caos-worker-bash-root";
          paths = [
            workerBashScript
            linuxPkgs.bash
            linuxPkgs.coreutils
            linuxPkgs.diffutils
            linuxPkgs.gnugrep
            linuxPkgs.findutils
          ];
        };
        workerBashConfig = {
          Entrypoint = [
            "/bin/caos"
            "runner"
          ];
          Env = [ "PATH=/bin" ];
        };
        workerBashImage = pkgs.dockerTools.buildImage {
          name = "caos-worker-bash";
          tag = "latest";
          fromImage = workerSharedBaseImage;
          copyToRoot = workerBashRoot;
          config = workerBashConfig;
        };


        # A real, runnable worker image, with a /worker that reads its inputs from
        # /cas/args (one entry per `--name=value` arg the run request carries), assembles
        # them into a result tree along with a small receipt, and stores that at
        # /cas/out. It runs via `caos runner`, which
        # populates /cas/args and runs /worker. This is the `worker-hello` crate, a
        # static binary at /worker — so the image needs no shell or coreutils.
        workerHelloRoot = workerRoot "worker-hello" worker-hello;
        workerHelloConfig = {
          Entrypoint = [
            "/bin/caos"
            "runner"
          ];
          Env = [ "PATH=/bin" ];
        };
        workerHelloImage = pkgs.dockerTools.buildImage {
          name = "caos-worker-hello";
          tag = "latest";
          fromImage = workerSharedBaseImage;
          copyToRoot = workerHelloRoot;
          config = workerHelloConfig;
        };

        # A "file-count" worker: counts the leaf files under `--in`, recursing
        # with itself through map-then. A tree (no `--children` yet) records the
        # continuation {in, map: file-count, then: file-count} and exits; called
        # back with `--children` (the then position) it sums the child counts; a
        # file counts as 1. The result, a blob holding the count, is left at
        # /cas/out. It reaches its own image at /cas/args/image (the request's
        # reserved entry). This is the `worker-file-count` crate, a static
        # binary at /worker — so the image needs no shell or coreutils.
        workerFileCountRoot = workerRoot "worker-file-count" worker-file-count;
        workerFileCountConfig = {
          Entrypoint = [
            "/bin/caos"
            "runner"
          ];
          Env = [ "PATH=/bin" ];
        };
        workerFileCountImage = pkgs.dockerTools.buildImage {
          name = "caos-worker-file-count";
          tag = "latest";
          fromImage = workerSharedBaseImage;
          copyToRoot = workerFileCountRoot;
          config = workerFileCountConfig;
        };

        # A "dirs-only" worker: keeps only a node's directory children, dropping
        # file (and other non-directory) children. It gets the node as `--in`
        # and leaves the filtered children tree at /cas/out (one entry per
        # surviving directory child, under its original name). Compose by
        # filtering first and recursing over the result. It only touches
        # the server (no sub-runs); the server injects that URL at runtime. This
        # is the `worker-dirs-only` crate, a static binary at /worker — so the
        # image needs no shell or coreutils.
        workerDirsOnlyRoot = workerRoot "worker-dirs-only" worker-dirs-only;
        workerDirsOnlyContents = [
          workerBaseRoot
          workerDirsOnlyRoot
        ];
        workerDirsOnlyConfig = {
          Entrypoint = [
            "/bin/caos"
            "runner"
          ];
          Env = [ "PATH=/bin" ];
        };
        workerDirsOnlyImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-dirs-only";
          tag = "latest";
          contents = workerDirsOnlyContents;
          config = workerDirsOnlyConfig;
          fakeRootCommands = installWorkerFiles;
        };

        # A "deep-deps" worker: turns a flat, name-keyed package map into a DAG of
        # deepened nodes. The input `packages` tree holds one subtree per
        # package, each with a `DEPS` blob (dependency names, one per line). The
        # output mirrors it, but each node carries a `DEEP-DEPS` subtree of its
        # recursively-deepened direct deps (which themselves carry DEEP-DEPS).
        #
        # It recurses through map-then, with this same image on both sides of
        # the continuation. `--mode` is optional; omitting it is the simple
        # public API:
        #   (no mode) — deepen one package (`--name`).
        #   all       — top-level convenience: deepen every package (a pure map).
        # The internal modes, reached only via curry by the driver:
        #   deepen — curried with `--packages` (the whole map): given a package
        #            as `--in`, resolve its `DEPS` names to the dep subtrees
        #            (pure CAS linking) and map-then itself over them.
        #   finish — given the package (curried as `--pkg`) and its deepened
        #            deps as `--children`, build the node (the package's files,
        #            minus DEPS, plus a DEEP-DEPS of the children).
        # Incrementality comes entirely from CAOS call memoization. deepen
        # carries the whole map, so it re-runs on any edit — cheap
        # orchestration. But finish (curried with only the package) is keyed on
        # a package and its deepened subgraph, so real recompute is O(changed
        # package + its dependents). A cycle re-enters the same deepen (image,
        # args) and is caught by the server's run-cycle detection.
        #
        # It reaches its own image (to curry deepen/finish) at /cas/args/image
        # (the request's reserved entry).
        #
        # This worker is the `worker-deep-deps` crate, a static binary placed at
        # /worker — so, like the other Rust workers, its image needs no shell or
        # coreutils, just caos (installed setuid by installWorkerFiles).
        workerDeepDepsRoot = workerRoot "worker-deep-deps" worker-deep-deps;
        workerDeepDepsConfig = {
          Entrypoint = [
            "/bin/caos"
            "runner"
          ];
          Env = [ "PATH=/bin" ];
        };
        workerDeepDepsImage = pkgs.dockerTools.buildImage {
          name = "caos-worker-deep-deps";
          tag = "latest";
          fromImage = workerSharedBaseImage;
          copyToRoot = workerDeepDepsRoot;
          config = workerDeepDepsConfig;
        };

        # A "rustc" worker: it *builds other workers*. Given a Rust source file as
        # `--src` and a worker-base git-docker image as `--base` (curried in), it
        # compiles the source (glibc/gnu, linking the vendored worker-common) and
        # emits a new worker image at /cas/out — the base's layers plus one carrying
        # the compiled /worker. The Rust + C toolchain is NOT baked in: it comes from
        # the stock rust base this image stacks on (see workerRustcRootEnv below).
        # This is the `worker-rustc` crate's binary at /worker; the worker-common
        # source is vendored at /vendor/worker-common for the in-image build's user
        # code to depend on.
        workerRustcRoot = workerRoot "worker-rustc" worker-rustc;
        # The worker-common crate source, for the in-image `cargo build` to link
        # user code against (it has no deps, so no registry/network is needed).
        workerCommonVendor = pkgs.runCommand "caos-worker-common-vendor" { } ''
          mkdir -p $out/vendor
          cp -r ${./crates/worker-common} $out/vendor/worker-common
        '';
        # The rustc worker's *delta*: only our bits — the /worker binary and the
        # vendored worker-common (caos is added setuid by installWorkerFilesBaseStacked).
        # The toolchain (cargo, rustc, gcc, glibc, coreutils, bash) comes from the
        # stock `rust:1-bookworm` base this stacks on (build-builtins.sh imports it
        # with `--base docker://rust:1-bookworm`), so none of it rides in git. Our
        # caos and /worker are musl-static, so they run unchanged on the glibc base.
        #
        # Built as a plain tree of *real* directories (not buildEnv, which would
        # symlink /worker etc. into /nix/store and, worse, make /etc a symlink that
        # shadows the base's real /etc). Real dirs overlay-MERGE with the base, so
        # the base keeps its /etc, /usr, … intact. We ship no /etc: the worker runs
        # as a numeric uid (caos drops to 1000 by setuid(2), no passwd entry needed)
        # and the base's own /etc (nsswitch, ssl certs, alternatives → cc) is kept.
        workerRustcRootEnv = pkgs.runCommand "caos-worker-rustc-root" { } ''
          mkdir -p $out/vendor
          cp ${workerRustcRoot}/worker $out/worker
          cp -r ${workerCommonVendor}/vendor/worker-common $out/vendor/worker-common
        '';
        workerRustcContents = [ workerRustcRootEnv ];
        # Env replicates the stock rust image's: /usr/local/cargo/bin (cargo/rustc)
        # first, then the usual debian dirs (/usr/local/bin holds our caos). The
        # server's convert uses this config verbatim, so it must be self-sufficient.
        # worker-rustc overrides CARGO_HOME per-build to /tmp/cargo (the only
        # world-writable spot), but a sane default doesn't hurt.
        workerRustcConfig = {
          Entrypoint = [
            "/bin/caos"
            "runner"
          ];
          Env = [
            "PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
            "RUSTUP_HOME=/usr/local/rustup"
            "CARGO_HOME=/usr/local/cargo"
          ];
          # This worker compiles Rust in release mode (rustc + LLVM + linker), so
          # it needs real RAM: at the 256MB default a build thrashes (~25s on fly);
          # at 2GB it's ~1s. caosd reads this label (caos.fly.memory-mb) when it
          # creates the worker machine, so the requirement rides with the image and
          # is the same on every stack — no stack-wide knob.
          Labels = {
            "caos.fly.memory-mb" = "2048";
          };
        };
        # buildLayeredImage so fakeRootCommands can install the setuid caos. With
        # the toolchain now in the stock rust base (not in git), this image is the
        # thin delta only — the published git-docker tree carries
        # `base = docker://rust:1-bookworm` plus these layers (build-builtins.sh).
        workerRustcImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-rustc";
          tag = "latest";
          contents = [ workerRustcRootEnv ];
          config = workerRustcConfig;
          fakeRootCommands = installWorkerFilesBaseStacked;
        };

        # The "runner": one warm, pooled image that runs a compiled worker binary
        # passed as the `bin` arg (see crates/worker-runner). Workers built from
        # source (by rustc) are produced as just a binary and curried into this
        # image, so they need no image of their own — no per-worker convert /
        # registry push / app provision, which is the cold-start cost. A thin delta
        # on the stock glibc base (debian:stable-slim, via build-builtins' --base),
        # carrying only the /worker trampoline; caos comes from
        # installWorkerFilesBaseStacked, libc + the rest from the base.
        workerRunnerRootEnv = pkgs.runCommand "caos-worker-runner-root" { } ''
          mkdir -p $out
          cp ${workerRoot "worker-runner" worker-runner}/worker $out/worker
        '';
        workerRunnerConfig = {
          Entrypoint = [
            "/bin/caos"
            "runner"
          ];
          Env = [
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
          ];
        };
        workerRunnerImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-runner";
          tag = "latest";
          contents = [ workerRunnerRootEnv ];
          config = workerRunnerConfig;
          fakeRootCommands = installWorkerFilesBaseStacked;
        };

        # The caos server: storage *and* compute in one process (it serves
        # /object from a git repo and /run by matching jobs to polling runners —
        # it runs no containers itself; that's runnerd's job). It shells out to
        # GNU `tar` to build layer tarballs when converting a git image, and
        # needs the git object database bind-mounted at /git (override with
        # CAOS_GIT_DIR):
        #   docker run --rm --network caos-net -p 9090:80 -v /repo/.git:/git \
        #     caos-server
        serverContents = [
          server
          # General-purpose Linux tools the server shells out to. Pulled from a
          # Linux nixpkgs (see linuxPkgs) so they're real ELF binaries — on macOS
          # they're substituted prebuilt from the cache rather than built.
          linuxPkgs.gnutar
          # `git http-backend` (and the `git` it dispatches): the smart-HTTP
          # transport the caos client uses as its `caos` remote. gitMinimal still
          # ships http-backend + core plumbing but drops git's python3/perl/docs
          # (~200 MiB) that the server never touches.
          linuxPkgs.gitMinimal
          # skopeo: copy a base image's blobs from its source registry into our
          # own repo, so a git image that references `base = docker://<ref>`
          # converts by stacking only its delta layers on top (no toolchain in
          # git). `cacert` gives skopeo a CA bundle for the TLS pulls.
          linuxPkgs.skopeo
          linuxPkgs.cacert
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

        # caos-runnerd: the generic runner — the one daemon that runs worker
        # containers. It long-polls the server for jobs and `docker run`s each
        # one, so it bundles the docker client and expects the host's docker
        # socket bind-mounted at /var/run/docker.sock:
        #   docker run --rm --network caos-net \
        #     -v /var/run/docker.sock:/var/run/docker.sock caos-runnerd
        runnerdContents = [
          runnerd
          # runnerd only ever shells out to `docker run`; it never builds or
          # composes, so drop the buildx + compose CLI plugins (~116 MiB).
          # Linux-only slimming: the override changes the drv hash, so the binary
          # cache can't substitute it — on macOS that would mean compiling docker
          # for Linux locally. There, ship the stock (cached) client instead.
          (if pkgs.stdenv.hostPlatform.isLinux then
            linuxPkgs.docker-client.override { buildxSupport = false; composeSupport = false; }
          else
            linuxPkgs.docker-client)
        ];
        runnerdConfig = {
          Cmd = [ "/bin/runnerd" ];
          Env = [ "PATH=/bin" ];
        };
        runnerdImage = pkgs.dockerTools.buildImage {
          name = "caos-runnerd";
          tag = "latest";
          copyToRoot = runnerdContents;
          config = runnerdConfig;
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
          contents = [ ];
          config = workerBaseConfig;
          fakeRootCommands = installWorkerFilesBaseStacked;
        };
        loadWorkerBash = loadImage {
          name = "caos-worker-bash";
          contents = [ workerBaseRoot workerBashRoot ];
          config = workerBashConfig;
          fakeRootCommands = installWorkerFiles;
        };
        loadServer = loadImage {
          name = "caos-server";
          contents = serverContents;
          config = serverConfig;
        };
        loadRunnerd = loadImage {
          name = "caos-runnerd";
          contents = runnerdContents;
          config = runnerdConfig;
        };
        loadWorkerHello = loadImage {
          name = "caos-worker-hello";
          contents = [ workerBaseRoot workerHelloRoot ];
          config = workerHelloConfig;
          fakeRootCommands = installWorkerFiles;
        };
        loadWorkerFileCount = loadImage {
          name = "caos-worker-file-count";
          contents = [ workerBaseRoot workerFileCountRoot ];
          config = workerFileCountConfig;
          fakeRootCommands = installWorkerFiles;
        };
        loadWorkerDirsOnly = loadImage {
          name = "caos-worker-dirs-only";
          contents = workerDirsOnlyContents;
          config = workerDirsOnlyConfig;
          fakeRootCommands = installWorkerFiles;
        };
        loadWorkerDeepDeps = loadImage {
          name = "caos-worker-deep-deps";
          contents = [ workerBaseRoot workerDeepDepsRoot ];
          config = workerDeepDepsConfig;
          fakeRootCommands = installWorkerFiles;
        };
        loadWorkerRustc = loadImage {
          name = "caos-worker-rustc";
          contents = [ workerRustcRootEnv ];
          config = workerRustcConfig;
          fakeRootCommands = installWorkerFilesBaseStacked;
        };
        loadWorkerRunner = loadImage {
          name = "caos-worker-runner";
          contents = [ workerRunnerRootEnv ];
          config = workerRunnerConfig;
          fakeRootCommands = installWorkerFilesBaseStacked;
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
        #
        # `caos-runnerd` joins them on Linux only. Every workspace crate is built
        # for musl (see CARGO_BUILD_TARGET), so the runnerd binary is a static
        # Linux ELF: on a Linux host it runs as-is, on macOS it can't execute and
        # would just be a dead entry on PATH. Running it on the host — rather than
        # as the compose stack's containerized runner — is what you want when the
        # daemon can't be handed a usable docker socket (rootless podman), since
        # it then inherits the shell's own `docker`/`podman` (CAOS_DOCKER_BIN).
        # Renamed on the way in: the crate is `runnerd`, but every host command
        # carries the `caos-` prefix, matching the image name and the README.
        caos-tools = pkgs.symlinkJoin {
          name = "caos-tools";
          paths = [
            caos-cli
            caosd
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ runnerd ];
          postBuild = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
            mv "$out/bin/runnerd" "$out/bin/caos-runnerd"
          '';
        };

        # The dev stack as docker compose: redis + registry + the caos server +
        # runnerd, mirroring the Tiltfile's wiring. The network and container
        # names are *pinned* (not compose's project-prefixed defaults) so the
        # worker containers runnerd spawns over the docker socket — which it
        # attaches to CAOS_DOCKER_NETWORK by this literal name — land on
        # caos-net and can reach caos-server. The server and runnerd images are
        # loaded from the Nix store (pull_policy: never), and CAOS_DATA
        # (absolute) holds the server's bare repo.
        composeFile = pkgs.writeText "docker-compose.yml" ''
          # caos dev stack — generated by the caos flake. Driven by `caosd`,
          # which sets CAOS_DATA + loads the server/runnerd images, then `up`s
          # this file and seeds the stdlib over HTTP (the server self-bootstraps
          # its bare repo on first boot). A bare `docker compose up` works once
          # CAOS_DATA is set and the images are loaded.
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
              volumes:
                - "''${CAOS_DATA:?set CAOS_DATA to an absolute data dir}/server-repo.git:/git"
              depends_on: [caos-redis, caos-registry]
            caos-runnerd:
              image: caos-runnerd:latest
              container_name: caos-runnerd
              pull_policy: never
              networks: [caos-net]
              environment:
                CAOS_DOCKER_NETWORK: caos-net
              volumes:
                - /var/run/docker.sock:/var/run/docker.sock
              depends_on: [caos-server]
        '';

        # The worker images that make up `std`, keyed by the same builtin name
        # build-builtins.sh maps them back to (via the <name> baked into each
        # tarball's store path). caosd hands these to build-builtins.sh so it
        # publishes the flake's own images without a runtime `nix build`.
        builtinWorkerImages = [
          workerBaseImage
          workerBashImage
          workerFileCountImage
          workerDirsOnlyImage
          workerHelloImage
          workerDeepDepsImage
          workerRustcImage
          workerRunnerImage
        ];

        # Bring the dev stack up (Ctrl-C tears it down). Loads the server image,
        # `docker compose up -d`, waits for the server (which self-bootstraps its
        # bare repo on first boot), then seeds the stdlib over HTTP with
        # `build-builtins.sh` — the SAME publish path fly and the tests use, so
        # there's one implementation. We hand build-builtins.sh a prebuilt
        # caos-cli, the flake's worker images, and a writable client repo (all via
        # env) so it needs neither `nix` nor a writable repo root at runtime —
        # hence caosd runs from any directory, including a tree that only imports
        # this flake. Uses the host's docker / `docker compose`; CAOS_DATA
        # (absolute) persists both the server repo and the publish client repo.
        caosd = pkgs.writeShellApplication {
          name = "caosd";
          runtimeInputs = [ pkgs.coreutils pkgs.git pkgs.curl pkgs.bash ];
          text = ''
            : "''${CAOS_DATA:=$PWD/.caos-data}"
            CAOS_DATA="$(readlink -m "$CAOS_DATA")"
            export CAOS_DATA
            mkdir -p "$CAOS_DATA"

            # Load the server/runnerd images only when this exact build isn't
            # already in docker. We tag the loaded image with a hash of its
            # (immutable) nix store path; the tag's presence means "this build is
            # loaded", so an unchanged restart skips the multi-second `docker
            # load`. Remove the image and the tag goes with it (reload); change
            # the image and its store path — hence the tag — changes (reload).
            load_once() {
              local name="$1" image="$2" src_tag
              src_tag="$name-src:$(printf '%s' "$image" | sha1sum | cut -c1-12)"
              if docker image inspect "$src_tag" >/dev/null 2>&1; then
                echo "==> $name image already loaded — skipping docker load" >&2
              else
                echo "==> loading $name image into docker" >&2
                docker load -i "$image"
                docker tag "$name:latest" "$src_tag"
              fi
            }
            load_once caos-server ${serverImage}
            load_once caos-runnerd ${runnerdImage}

            echo "==> starting stack (redis, registry, server, runnerd)" >&2
            trap 'docker compose -f ${composeFile} down' EXIT
            docker compose -f ${composeFile} up -d

            # The server self-bootstraps an empty /git on first boot; wait for it,
            # then publish the stdlib over HTTP (build-builtins caches imports under
            # refs/caos/src, so a restart re-seeds in ~seconds).
            echo "==> waiting for caos-server on :9090 ..." >&2
            for _ in $(seq 1 60); do
              curl -s -o /dev/null --max-time 2 http://localhost:9090/ && break
              sleep 1
            done
            echo "==> publishing stdlib (build-builtins.sh)" >&2
            CAOS_SERVER_URL=http://localhost:9090 \
            CAOS_CLI=${caos-cli}/bin/caos-cli \
            CAOS_CLIENT_REPO="$CAOS_DATA/publish-client-repo" \
            CAOS_BUILTIN_IMAGES="${
              pkgs.lib.concatMapStringsSep " " toString builtinWorkerImages
            }" \
              bash ${self}/build-builtins.sh >/dev/null

            echo "==> stack up — Ctrl-C to stop. Following logs:" >&2
            docker compose -f ${composeFile} logs -f
          '';
        };
      in
      {
        packages = {
          default = caos;
          inherit caos server runnerd caos-cli caosd caos-tools;

          # The generated compose file, for driving the stack by hand
          # (`docker compose -f $(nix build --print-out-paths .#docker-compose)
          # up`). `caosd` is the batteries-included way.
          docker-compose = composeFile;

          # Image tarballs (build with `nix build`, then `docker load < result`).
          caos-worker-base-docker = workerBaseImage;
          caos-worker-bash-docker = workerBashImage;
          caos-server-docker = serverImage;
          caos-runnerd-docker = runnerdImage;
          caos-worker-hello-docker = workerHelloImage;
          caos-worker-file-count-docker = workerFileCountImage;
          caos-worker-dirs-only-docker = workerDirsOnlyImage;
          caos-worker-deep-deps-docker = workerDeepDepsImage;
          caos-worker-rustc-docker = workerRustcImage;
          caos-worker-runner-docker = workerRunnerImage;
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
          load-caos-runnerd = {
            type = "app";
            program = "${loadRunnerd}/bin/load-caos-runnerd";
          };
          load-caos-worker-hello = {
            type = "app";
            program = "${loadWorkerHello}/bin/load-caos-worker-hello";
          };
          load-caos-worker-file-count = {
            type = "app";
            program = "${loadWorkerFileCount}/bin/load-caos-worker-file-count";
          };
          load-caos-worker-dirs-only = {
            type = "app";
            program = "${loadWorkerDirsOnly}/bin/load-caos-worker-dirs-only";
          };
          load-caos-worker-deep-deps = {
            type = "app";
            program = "${loadWorkerDeepDeps}/bin/load-caos-worker-deep-deps";
          };
          load-caos-worker-rustc = {
            type = "app";
            program = "${loadWorkerRustc}/bin/load-caos-worker-rustc";
          };
          load-caos-worker-runner = {
            type = "app";
            program = "${loadWorkerRunner}/bin/load-caos-worker-runner";
          };
        };

        checks = {
          inherit caos server runnerd;

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
            # `fly` CLI: auth (`fly auth token`), org/region lookup, and operating
            # the fly backend (apps, machines, logs). caosd itself talks to the
            # Machines API + registry over HTTP and does not need this.
            pkgs.flyctl
          ];
        };
      }
    );
}
