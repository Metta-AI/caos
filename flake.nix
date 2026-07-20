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
        worker-cargo = crateBin "worker-cargo";
        # The agent-harness workers (design/agent-harness.md). They have no
        # image of their own: each runs as curry(runner, bin=<static binary>)
        # in the shared runner pool, so only the binaries are exposed.
        worker-bash-tool = crateBin "worker-bash-tool";
        worker-llm-step = crateBin "worker-llm-step";
        worker-rgrep = crateBin "worker-rgrep";
        # The llm-step tests' scripted LLM API stand-in — a host binary, not a
        # worker (the musl build runs on any Linux host).
        llm-stub = crateBin "llm-stub";

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
        # glibc/coreutils come from the Debian base. Workers stacked on it
        # inherit this setuid caos layer by hash, so an unprivileged builder
        # never has to synthesize a setuid-root binary itself.
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

        # The "rustc" worker *builds other workers*, but carries no toolchain:
        # it is pure orchestration over the cargo worker (lay out the project,
        # run-then into cargo-base, curry the produced binary into the runner
        # — see crates/worker-rustc and design/cargo-workers.md). It runs as
        # `curry(runner, bin=worker-rustc)` in the shared pool like the other
        # source-level workers; build-builtins.sh curries in the cargo worker
        # and the worker-common source tree. The old rust:1-bookworm-based
        # rustc image is retired.

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

        # The "testenv" worker (design/cargo-workers.md, phase 3): the bash
        # script worker's sibling, for jobs that run a whole INNER caos stack
        # — a per-test job starts the edited server + a process-mode runnerd
        # (chroot slots) inside its own container and drives a test against
        # them. Differences from `bash`: git rides along (the inner server's
        # smart-HTTP transport and the inner client repo need it), and the
        # config grants CAOS_WORKER_UID=0 — the script runs as ROOT, which the
        # inner stack requires (setuid installs into the slots, chroot). That
        # grant is per-image containment policy: only jobs run on THIS image
        # get it; every other worker keeps the uid-1000 fence.
        workerTestenvRoot = pkgs.buildEnv {
          name = "caos-worker-testenv-root";
          paths = [
            workerBashScript
            linuxPkgs.bash
            linuxPkgs.coreutils
            linuxPkgs.diffutils
            linuxPkgs.gnugrep
            linuxPkgs.findutils
            linuxPkgs.gitMinimal
            # The docker (moby) client, so an inner runnerd in this worker can
            # delegate sibling containers to the outer engine over the granted
            # socket (phase 4). Same slimmed client the runnerd image ships.
            (if pkgs.stdenv.hostPlatform.isLinux then
              linuxPkgs.docker-client.override { buildxSupport = false; composeSupport = false; }
            else
              linuxPkgs.docker-client)
          ];
        };
        workerTestenvConfig = {
          Entrypoint = [
            "/bin/caos"
            "runner"
          ];
          Env = [
            "PATH=/bin"
            # The root grant (see above): caos runner reads these and skips
            # the uid-1000 drop for this image's jobs.
            "CAOS_WORKER_UID=0"
            "CAOS_WORKER_GID=0"
          ];
        };
        workerTestenvImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-testenv";
          tag = "latest";
          contents = [
            workerTestenvRoot
            workerBaseRoot
          ];
          config = workerTestenvConfig;
          fakeRootCommands = installWorkerFiles;
        };

        # The cargo toolchain BASE image (design/cargo-workers.md, phases 0–1):
        # the pinned toolchain + the workspace's deps pre-compiled, with the
        # runner trampoline at /worker. The cargo worker itself is published as
        # `curry(cargo-base, bin=worker-cargo)` (build-builtins.sh) — the
        # runner-pool move — so this image is keyed on (toolchain, lockfile)
        # only and a caos source change ships one small binary blob, never a
        # re-import of these layers. Unlike
        # rustc (thin delta on a stock base), this image is SELF-CONTAINED nix:
        # cargo fingerprints are keyed on the exact compiler build, and dep
        # artifacts contain proc-macro dylibs / build-script binaries linked
        # against the compiling toolchain's glibc — so the toolchain that baked
        # the deps must be the toolchain that uses them. A stock rust base
        # can't satisfy that; the pinned nix toolchain can. The cost is a big
        # image through the git import, paid once per toolchain/lockfile bump.
        #
        # `minimal` (rustc+cargo+host std) keeps clippy/rustfmt/rust-src out of
        # the image; it resolves the same version as rust-toolchain.toml's
        # `stable` channel because both come from the one flake.lock'd
        # rust-overlay revision. The musl std rides along so produced binaries
        # (rustc-built user workers) can be static — they then run on any base
        # (the debian-slim runner today, scratch eventually).
        cargoWorkerToolchain = linuxPkgs.rust-bin.stable.latest.minimal.override {
          targets = [ muslTarget ];
        };
        craneLibCargoWorker = (crane.mkLib linuxPkgs).overrideToolchain cargoWorkerToolchain;

        # The vendored crates.io sources for the workspace's Cargo.lock, plus
        # crane's source-replacement config pointing at them. A store path —
        # the same absolute path at bake time and in-container, which the
        # fingerprints require.
        cargoWorkerVendor = craneLibCargoWorker.vendorCargoDeps { inherit src; };

        # Every workspace dependency pre-compiled (check + build + test-deps,
        # dev profile) against crane's DUMMY workspace sources — keyed on
        # manifests + lockfile only, so source edits never re-bake. The worker
        # re-materializes real sources at the same absolute root with fresh
        # mtimes: deps stay fingerprint-fresh, workspace crates always rebuild.
        cargoWorkerDeps = craneLibCargoWorker.buildDepsOnly {
          inherit src;
          pname = "caos-cargo";
          version = "0.1.0";
          strictDeps = true;
          cargoVendorDir = cargoWorkerVendor;
          # dev profile: what the worker's plain `cargo check/build/test` use.
          CARGO_PROFILE = "dev";
          # Smaller debuginfo (file:line in backtraces, no full DWARF). The
          # image env repeats it: a profile mismatch is a silent full rebuild.
          CARGO_PROFILE_DEV_DEBUG = "line-tables-only";
          cargoExtraArgs = "--locked --workspace";
          # Record the sandbox workspace root: fingerprints are absolute-path
          # keyed, so the worker must rebuild the workspace at this exact path
          # (the image inflates target/ there; the worker reads /ws-root).
          postInstall = ''echo -n "$PWD" > $out/ws-root'';
        };

        # The image root: the runner trampoline at /worker (the actual cargo
        # worker binary arrives as the curried `bin` arg) plus the baked
        # target/ inflated at the exact workspace root the bake used. The
        # toolchain, cc and vendor ride the image closure via the config
        # references below.
        cargoBaseRootEnv =
          pkgs.runCommand "caos-worker-cargo-base-root" { nativeBuildInputs = [ pkgs.zstd ]; }
            ''
              mkdir -p $out
              cp ${workerRoot "worker-runner" worker-runner}/worker $out/worker
              wsroot=$(cat ${cargoWorkerDeps}/ws-root)
              mkdir -p "$out$wsroot"
              tar --zstd -xf ${cargoWorkerDeps}/target.tar.zst -C "$out$wsroot"
              cp ${cargoWorkerDeps}/ws-root $out/ws-root
            '';
        cargoBaseConfig = {
          Entrypoint = [
            "/bin/caos"
            "runner"
          ];
          Env = [
            # The pinned toolchain and a C linker (for build/test binaries; the
            # same cc-wrapper the bake's build scripts and proc macros linked
            # under, so everything resolves against one glibc).
            "PATH=${cargoWorkerToolchain}/bin:${linuxPkgs.stdenv.cc}/bin:/bin"
            # A writable home; the worker copies the vendor config here.
            "CARGO_HOME=/tmp/cargo"
            "CAOS_VENDOR_CONFIG=${cargoWorkerVendor}/config.toml"
            # Must match the bake (see cargoWorkerDeps).
            "CARGO_PROFILE_DEV_DEBUG=line-tables-only"
          ];
        };
        cargoBaseImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-cargo-base";
          tag = "latest";
          contents = [
            cargoBaseRootEnv
            workerBaseRoot
          ];
          config = cargoBaseConfig;
          fakeRootCommands = installWorkerFiles + ''
            # The workspace root must be writable by the (uid 1000) worker:
            # it materializes sources there and cargo writes target/.
            wsroot=$(cat ws-root)
            chown -R 1000:1000 ".$wsroot"
          '';
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
        # Installed under both names: `caos` is what a person types (`caos talk`),
        # `caos-cli` stays for scripts and docs that spell it out. (No collision
        # with the worker-side `caos` binary — that one is baked into images and
        # never lands on a host PATH.)
        caos-cli =
          if pkgs.stdenv.hostPlatform.isLinux then
            pkgs.runCommand "caos-cli" { } ''
              mkdir -p $out/bin
              cp ${caos}/bin/caos-cli $out/bin/caos-cli
              ln -s caos-cli $out/bin/caos
            ''
          else
            craneLib.buildPackage (
              nativeArgs
              // {
                cargoArtifacts = craneLib.buildDepsOnly nativeArgs;
                cargoExtraArgs = "--package caos --bin caos-cli";
                doCheck = false;
                postInstall = "ln -s caos-cli $out/bin/caos";
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
        # (absolute) holds all persistent state (server repo, redis, registry).
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
          # All persistent state bind-mounts under CAOS_DATA — one dir to inspect,
          # back up, or wipe. Each service keeps its image's default user: server
          # and registry run as root (host-owned files under rootless podman);
          # redis drops to uid 999, so its dir ends up owned by a host subuid and
          # cleaning it needs `sudo rm` (or `podman unshare rm`) — fine for a
          # throwaway dev cache.
          services:
            caos-redis:
              image: redis:7
              container_name: caos-redis
              networks: [caos-net]
              ports: ["6379:6379"]
              # Persist the result cache across restarts; appendonly keeps it
              # durable to a hard kill, not just a clean SIGTERM shutdown-save.
              command: ["redis-server", "--appendonly", "yes"]
              volumes:
                - "''${CAOS_DATA:?set CAOS_DATA to an absolute data dir}/redis:/data"
            caos-registry:
              image: registry:2
              container_name: caos-registry
              networks: [caos-net]
              ports: ["5000:5000"]
              # Persist converted worker images so the first run of each after a
              # restart is a registry hit, not a re-convert + re-push.
              volumes:
                - "''${CAOS_DATA:?set CAOS_DATA to an absolute data dir}/registry:/var/lib/registry"
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
                # Pass the engine socket through to workers that ask for it, so a
                # worker's own inner runnerd can launch sibling containers via the
                # same engine (phase 4, design/cargo-workers.md). The host socket
                # (bind source resolves on the host, docker-out-of-docker) is
                # mounted into each worker at /run/caos/engine.sock. Coarse for now
                # — every worker gets it; a per-image grant is future work.
                CAOS_RUNNER_SOCKET: /var/run/docker.sock
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
          workerRunnerImage
          cargoBaseImage
          workerTestenvImage
        ];

        # The agent-harness worker binaries build-builtins.sh publishes as
        # std curries over the runner image (std/bash-tool, std/llm-step) —
        # handed over prebuilt for the same no-runtime-nix reason as the images.
        builtinWorkerBins = [
          worker-bash-tool
          worker-llm-step
          worker-rgrep
          # Published as curry(cargo-base, bin) — see bin_base in
          # build-builtins.sh.
          worker-cargo
          # Published as curry(runner, bin) with the cargo worker and the
          # worker-common source curried in.
          worker-rustc
        ];

        # The dev stack's control command. Subcommands:
        #   caosd up     (default) idempotently bring the stack up and publish all
        #                of std, then RETURN — the stack stays running in the
        #                background. Fast on a warm stack (~3s: images already
        #                loaded, the std publish is a cache hit), so callers (the
        #                tests, a consuming tree) just run it to guarantee a
        #                current stack — no daemon to babysit, no teardown race.
        #   caosd down   stop the stack (all CAOS_DATA state kept).
        #   caosd reset  stop and wipe CAOS_DATA state for a clean slate.
        #   caosd logs   follow the running stack's logs (Ctrl-C returns; the stack
        #                keeps running).
        # `up` hands build-builtins.sh a prebuilt caos-cli, the flake's worker
        # images, and a writable client repo (all via env) so it needs neither
        # `nix` nor a writable repo root — hence it runs from any directory,
        # including a tree that only imports this flake. This is the SAME
        # std-publish path fly and the tests use, so there's one implementation.
        # Uses the host's docker / `docker compose`; CAOS_DATA (absolute) holds all
        # persistent state — server repo, publish client repo, redis, registry.
        caosd = pkgs.writeShellApplication {
          name = "caosd";
          runtimeInputs = [ pkgs.coreutils pkgs.git pkgs.curl pkgs.bash ];
          text = ''
            : "''${CAOS_DATA:=$PWD/.caos-data}"
            CAOS_DATA="$(readlink -m "$CAOS_DATA")"
            export CAOS_DATA
            mkdir -p "$CAOS_DATA"

            compose() { docker compose -f ${composeFile} "$@"; }

            case "''${1:-up}" in
            up)
              # Load the server/runnerd images only when this exact build isn't
              # already in docker. We tag the loaded image with a hash of its
              # (immutable) nix store path; the tag's presence means "this build is
              # loaded", so an unchanged restart skips the multi-second `docker
              # load`. Remove the image and the tag goes with it (reload); change
              # the image and its store path — hence the tag — changes (reload).
              # Load each daemon image unless this exact build is already loaded.
              # The src tag is content-addressed (sha1 of the image's immutable
              # store path), so an unchanged build skips the multi-second docker
              # load; a changed build has a new store path — hence a new tag — and
              # loads. Either way <svc>:latest ends up pointing at the wanted build.
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

              # A running container is stale when its image id != the build we just
              # made <svc>:latest — i.e. the nix package changed under a container
              # that's still up. `compose up -d` won't catch this (podman-compose
              # keys "up-to-date" off the container name/config, not its image id),
              # so compare the hashes ourselves and collect the mismatches. A
              # container-less service returns early: `compose up -d` creates it
              # fresh on :latest, so it's never stale.
              stale=()
              check_current() {
                local svc="$1" have want
                have=$(docker inspect -f '{{.Image}}' "$svc" 2>/dev/null) || return 0
                want=$(docker image inspect -f '{{.Id}}' "$svc:latest" 2>/dev/null || true)
                [ -n "$want" ] && [ "$want" != "$have" ] && stale+=("$svc")
                return 0
              }
              load_once caos-server ${serverImage};   check_current caos-server
              load_once caos-runnerd ${runnerdImage}; check_current caos-runnerd

              # up -d is idempotent: a no-op when the stack is already running, and
              # it creates only what's missing. No teardown trap — `up` returns with
              # the stack still up (stop it with `caosd down`).
              echo "==> starting stack (redis, registry, server, runnerd)" >&2
              compose up -d
              # Recreate exactly the services running a stale image — nothing else,
              # so an unchanged `up` (and a freshly-created stack) keeps its
              # containers in place.
              if [ "''${#stale[@]}" -gt 0 ]; then
                echo "==> recreating onto rebuilt image(s): ''${stale[*]}" >&2
                compose up -d --force-recreate "''${stale[@]}"
              fi

              # The server self-bootstraps an empty /git on first boot; wait for it,
              # then publish the stdlib over HTTP (build-builtins caches imports under
              # refs/caos/src, so a re-publish is a near-instant cache hit).
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
              CAOS_BUILTIN_BINS="${
                pkgs.lib.concatMapStringsSep " " toString builtinWorkerBins
              }" \
                bash ${self}/build-builtins.sh >/dev/null

              echo "==> stack up. 'caosd logs' to follow, 'caosd down' to stop." >&2
              ;;
            down)
              echo "==> stopping stack (CAOS_DATA kept; 'caosd reset' wipes it)" >&2
              compose down
              ;;
            reset)
              # All state is bind-mounted under CAOS_DATA. redis runs as uid 999,
              # so its dir is owned by a host subuid a plain rm can't remove;
              # finish that with `sudo rm -rf` (or `podman unshare rm -rf`).
              echo "==> stopping stack and wiping CAOS_DATA state" >&2
              compose down
              rm -rf "$CAOS_DATA/server-repo.git" "$CAOS_DATA/publish-client-repo" \
                     "$CAOS_DATA/redis" "$CAOS_DATA/registry" 2>/dev/null || true
              ;;
            logs)
              # -t: runtime-recorded per-line timestamps — the daemons don't
              # stamp their own lines, and a turn timeline needs them.
              compose logs -f -t
              ;;
            *)
              echo "caosd: unknown command '$1'" >&2
              echo "usage: caosd [up|down|reset|logs]" >&2
              exit 2
              ;;
            esac
          '';
        };

        # A thin `caosd` for the dev shell. It defers to `nix run` against the
        # working-tree flake, so caosd's image closure (server/runnerd/worker
        # images) builds lazily on the first `caosd up` — exactly as running
        # `nix run .#caosd` by hand would — instead of being dragged onto the
        # dev-shell's critical path at shell entry. Resolves the flake from the
        # git top level so it works from any subdirectory, and passes args
        # through, so up/down/reset/logs all work. Note: it always runs the
        # *current* checkout (a dirty tree just prints nix's "dirty" warning).
        caosd-launcher = pkgs.writeShellScriptBin "caosd" ''
          exec nix run "$(${pkgs.git}/bin/git rev-parse --show-toplevel)#caosd" -- "$@"
        '';
      in
      {
        packages = {
          default = caos;
          inherit caos server runnerd caos-cli caosd caos-tools;
          # Agent-harness worker binaries (run as curry(runner, bin)) and the
          # llm-step tests' stub LLM server.
          inherit worker-bash-tool worker-llm-step worker-rgrep llm-stub;
          # The cargo worker (curry(cargo-base, bin)) and the rustc
          # orchestrator (curry(runner, bin)) — build-builtins.sh needs the
          # binaries exposed.
          inherit worker-cargo worker-rustc;
          # The runner-pool trampoline binary, exposed for the process-mode
          # backend (it becomes each chroot slot's /worker; tests/proc-stack).
          inherit worker-runner;
          # The pure-Rust example workers' binaries, exposed for the inner
          # process-mode suite (curry(dummy, bin); tests/suite-in-caos).
          inherit worker-file-count worker-dirs-only worker-deep-deps;

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
          caos-worker-runner-docker = workerRunnerImage;
          caos-worker-cargo-base-docker = cargoBaseImage;
          caos-worker-testenv-docker = workerTestenvImage;
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
            # `caosd` on PATH as a thin launcher (see caosd-launcher): it defers to
            # `nix run .#caosd`, so the stack's image closure builds lazily on the
            # first `caosd up`, not at dev-shell entry.
            caosd-launcher
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
