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

          # Build everything at the dev profile (opt-level 0, no LTO, many
          # codegen units). These are dev-stack / local artifacts, so we trade
          # runtime speed and binary size for much faster builds — the whole
          # point of this flake path. musl still links static at any profile
          # (crt-static is on), so the binaries still run on any base.
          # line-tables-only keeps file:line backtraces without baking full
          # DWARF (plain dev debuginfo) into every image. This mirrors the cargo
          # worker's bake (cargoBake.deps, std/cargo/bake.nix), which
          # already builds dev.
          CARGO_PROFILE = "dev";
          CARGO_PROFILE_DEV_DEBUG = "line-tables-only";

          # Native build inputs / runtime libs go here as the project grows,
          # e.g. pkgs.openssl + pkgs.pkg-config for TLS. Note: C deps would
          # need a musl cross-toolchain to stay static. (the server's gix
          # uses default-features = false, so it stays pure-Rust / static.)
          # buildInputs = [ ];
          # nativeBuildInputs = [ ];
        }
        // crossLinkerEnv;

        # Build the compute/runtime workspace dependencies once and cache them
        # separately from the crates.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # ONE build for every binary. Each crate used to be its own
        # `buildPackage (--package <name>)`, but all of them took the same
        # whole-tree `src`, so any edit re-keyed and rebuilt every one of them —
        # the split bought no incrementality while paying the per-binary compile
        # + link N times over. A single `--workspace` build does the shared work
        # once and parallelizes across cores. Images stay single-binary: each
        # consumer copies one *named* binary out of this output's /bin, so
        # nothing extra ever lands in an image.
        workspaceBins = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            cargoExtraArgs = "--workspace";
            doCheck = false;
          }
        );

        # Every crate's binary is selected (by name, at copy time) from the one
        # build above, so these are all the same derivation. `caos` carries two
        # binaries — `caos` (worker-side, baked into images) and `caos-cli`
        # (user-facing) — both in /bin; consumers pick the one they need.
        caos = workspaceBins;
        server = workspaceBins;
        runnerd = workspaceBins;
        worker-rustc = workspaceBins;
        worker-runner = workspaceBins;
        worker-cargo = workspaceBins;
        # The agent-harness workers (design/agent-harness.md). They have no
        # image of their own: each runs as curry(runner, bin=<static binary>)
        # in the shared runner pool, so only the binaries are exposed.
        worker-bash-tool = workspaceBins;
        worker-llm-step = workspaceBins;
        worker-rgrep = workspaceBins;
        # The llm-step tests' scripted LLM API stand-in — a host binary, not a
        # worker (the musl build runs on any Linux host).
        llm-stub = workspaceBins;

        # Minimal images: each contains *only* its static binary — no shell, no
        # libc, no /nix/store. Crates are unprefixed (caos, server) but
        # the published image names carry a `caos-` prefix.
        # The images are Linux, but build on macOS too (no VM): the Rust binaries
        # cross-compile for the host arch via rust-lld (see muslCrossLinker), and
        # the server image's tools come from linuxPkgs — substituted prebuilt from
        # the binary cache.

        # Two worker images live here — the host-built streamed core: the
        # flake-builder (below) and the runner (workerRunnerImage). Every
        # other std entry is a literal checked-in flake tree (std/{cargo,
        # bash,testenv} — design/flake-images.md part 2), each defining its
        # own /worker, or a curry(runner, worker1=<static musl binary>)
        # (builtinWorkerBins below).
        #
        # Both are built CLEAN — no caos, no user db, no /tmp. The caos
        # additions (and the runner's /worker) are composed on at publish
        # by build-builtins.sh as CONTENT-KEYED tar layers over these
        # images: the same clean-image + additions-delta shape the stack
        # stage gives every flake image, and the reason a workspace rebuild
        # that leaves the binaries' bytes unchanged re-streams nothing.
        # (Nix could not bake the additions anyway — it strips setuid when
        # it seals a store path — and nix-side baking keyed them on store
        # PATHS, which rename on every rebuild.)

        # The flake-builder (design/flake-images.md): the SOLE bootstrap
        # builtin — the one image the flake path can't build (it IS the
        # flake path), so the host builds it. Its definition is
        # std/flake-builder's own flake, taken AS-IS: we call its outputs
        # function directly — the standard subflake call, no path-input
        # lock churn — passing OUR nixpkgs (tests/std-lint pins the
        # subflake's lock to the same revision). Its clean #caosImage is
        # exactly what streams.
        workerFlakeBuilderImage =
          ((import ./std/flake-builder/flake.nix).outputs {
            self = null;
            inherit nixpkgs;
          }).packages.${linuxSystem}.caosImage;

        # std/runner as HOST-BUILT streamed core (design/flake-images.md,
        # part 2): the pooled interpreter IMAGE every compiled worker runs
        # on — not a worker itself (its /worker awaits a worker1; the
        # workers are the curries over it). worker-runner is the in-image
        # half of the runner-pool protocol — it versions with the client
        # and the protocol, not with std content — so the host builds and
        # streams this image; the std entry is a curry over the digest.
        # The image here is the USERLAND ONLY: /worker rides in at publish
        # as a content-keyed layer next to the additions (build-builtins),
        # so this derivation depends on nothing from the workspace and a
        # Rust edit never re-keys it.
        workerRunnerImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-runner";
          tag = "latest";
          # bash provides /bin/sh too. The userland is what compiled workers
          # — and the commands bash-tool runs for the agent — see at runtime.
          contents = [
            linuxPkgs.bash
            linuxPkgs.coreutils
            linuxPkgs.diffutils
            linuxPkgs.gnugrep
            linuxPkgs.gnused
            linuxPkgs.findutils
            linuxPkgs.gnutar
            linuxPkgs.gzip
          ];
          config = {
            # runnerd forces the entrypoint anyway; set it so the streamed
            # compose (which renders config.Entrypoint into a Dockerfile)
            # always has one.
            Entrypoint = [
              "/bin/caos"
              "runner"
            ];
            Env = [ "PATH=/bin" ];
          };
        };

        # The cargo toolchain + workspace-deps bake (design/cargo-workers.md,
        # phases 0–1): ONE definition in std/cargo/bake.nix, shared with
        # the published std/cargo flake (finding B, design/flake-images.md)
        # — the flake-builder images that tree into the base std/cargo curries
        # onto, so nothing here rides the std publish. The published flake's
        # lock is derived from THIS flake's lock (std/refresh.sh writes the
        # checked-in copy; tests/std-lint verifies it), so both sides evaluate
        # the same expression against the same pins and cannot drift.
        cargoBake = import ./std/cargo/bake.nix {
          pkgs = linuxPkgs;
          inherit crane src;
          toolchainFile = ./rust-toolchain.toml;
        };

        # The DEPS-ONLY cargo base (phase D2): the bake + env, WITHOUT caos or
        # a /worker — those are stacked on by the suite's image
        # jobs from the freshly caos-built binaries (the D1 delta-over-base
        # move). So this image is keyed on (toolchain, manifests, lockfile)
        # alone, and the expensive in-caos nix bake that produces it re-runs
        # only when those change — never on a source edit.
        cargoDepsImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-cargo-deps";
          tag = "latest";
          # bash + coreutils (+ the /bin/sh link below): a bare nix-rooted
          # image has neither a shell nor chmod/ln, and the suite's image job
          # stacks the delta with Dockerfile RUN steps that need both.
          contents = [
            cargoBake.rootEnv
            linuxPkgs.bashInteractive
            linuxPkgs.coreutils
          ];
          config = {
            Env = cargoBake.env;
          };
          fakeRootCommands = cargoBake.inflate + ''
            ln -sf bash bin/sh
            # Workers (uid 1000) scratch under /tmp; a bare nix root has none.
            mkdir -p tmp
            chmod 1777 tmp
          '';
        };

        # ---- The test stack (design/test-stack-image.md) ----
        # THE image this flake defines. Handed to std/flake-builder, this tree
        # yields one image that hosts a COMPLETE caos stack built from itself:
        # the binaries, the userland the stack shells out to, the tree's own
        # clean core images, and an interpreter /worker that brings the stack
        # up and then runs `worker1` against it. The suite runs it once per
        # test; `caos-tools/build.sh` collapses to building it.
        #
        # The caos ADDITIONS (setuid /bin/caos, the user db, /tmp) are not
        # here: the flake-builder's stack stage composes them, from ITS caos —
        # the host's — which is right, because this image runs as a worker on
        # the OUTER stack. The tested caos lives at /caos/bin and reaches
        # worker1 by PATH, set at the call site (test-stack/worker).
        testStackBins = [
          "caos" "caos-cli" "server" "runnerd"
          "worker-cargo" "worker-runner" "worker-rustc"
          "worker-bash-tool" "worker-llm-step" "worker-rgrep"
          # The llm-step tests' scripted API stand-in — a test helper the
          # inner suite hands to its jobs, not a worker.
          "llm-stub"
        ];
        testStackRoot = pkgs.runCommand "caos-test-stack-root" { } ''
          mkdir -p $out/caos/bin $out/caos/images $out/caos/tree
          for b in ${pkgs.lib.concatStringsSep " " testStackBins}; do
            cp ${workspaceBins}/bin/$b $out/caos/bin/$b
          done
          # Store BASENAMES intact: build-builtins.sh maps a tarball back to
          # its builtin by the caos-worker-<name> baked into the path.
          cp ${workerFlakeBuilderImage} \
            $out/caos/images/${baseNameOf "${workerFlakeBuilderImage}"}
          cp ${workerRunnerImage} \
            $out/caos/images/${baseNameOf "${workerRunnerImage}"}
          cp -R ${./std} $out/caos/tree/std
          # std/rustc is curry(runner, worker1=worker-rustc, cargo=<ref>,
          # worker_common=<source tree>) — the publisher stages that source,
          # so the tree needs it alongside std/.
          mkdir -p $out/caos/tree/crates
          cp -R ${./crates/worker-common} $out/caos/tree/crates/worker-common
          install -m 755 ${./build-builtins.sh} $out/caos/tree/build-builtins.sh
          install -m 755 ${./test-stack/worker} $out/worker
        '';
        testStackImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-test-stack";
          tag = "latest";
          contents = [
            testStackRoot
            # The userland the stack itself shells out to: git (the server's
            # smart-HTTP transport and the publish client), skopeo + certs
            # (registry copies), redis (the private inner result cache), the
            # docker client (the inner runnerd delegating to the outer
            # engine), and a shell + the usual tools for the scripts that run
            # here.
            linuxPkgs.bashInteractive
            linuxPkgs.coreutils
            # cmp/diff: the tests compare cached results byte for byte, and
            # std/refresh.sh --check re-derives and diffs every checked-in
            # std copy. This image is now the environment every test runs in,
            # so it inherits what std/testenv carried.
            linuxPkgs.diffutils
            linuxPkgs.gnugrep
            linuxPkgs.gnused
            linuxPkgs.findutils
            linuxPkgs.gnutar
            linuxPkgs.gzip
            linuxPkgs.curl
            linuxPkgs.jq
            # build-builtins.sh reads the registry's manifest digest out of a
            # curl -I with awk, and coreutils has none.
            linuxPkgs.gawk
            linuxPkgs.gitMinimal
            linuxPkgs.redis
            linuxPkgs.skopeo
            linuxPkgs.cacert
            (if pkgs.stdenv.hostPlatform.isLinux then
              linuxPkgs.docker-client.override { buildxSupport = false; composeSupport = false; }
            else
              linuxPkgs.docker-client)
          ];
          config = {
            Entrypoint = [ "/bin/caos" "runner" ];
            Env = [
              "PATH=/bin"
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              # Root: this worker starts daemons, owns an inner git dir, and
              # drives the engine socket — the same per-image containment
              # grant the flake-builder and testenv carry.
              "CAOS_WORKER_UID=0"
              "CAOS_WORKER_GID=0"
            ];
          };
        };

        # The test stack's deps memo (design/flake-deps-image.md). Everything
        # here is SOURCE-INDEPENDENT — the dep bake rides crane's dummy
        # sources, and the two core images depend on nothing from the
        # workspace — so a caos edit leaves this derivation where it is and
        # the builder starts from a warm store.
        testStackDepsRegistration = pkgs.runCommand "caos-test-stack-deps-registration" { } ''
          mkdir -p $out
          cp ${
            pkgs.closureInfo {
              rootPaths = [
                cargoArtifacts
                rustToolchain
                workerFlakeBuilderImage
                workerRunnerImage
              ];
            }
          }/registration $out/caos-deps-registration
        '';
        testStackDepsImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-test-stack-deps";
          tag = "latest";
          contents = [ testStackDepsRegistration ];
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

        # ---- Cross-tree consumption: caos-cli, the stack, the stdlib ----
        # These let another tree (one that has caos as a flake input) get the
        # user-facing CLI on its PATH, bring the dev stack up, and publish the
        # builtin worker library — without the caos source tree.

        # Just the user-facing CLI (a consumer wants only `caos-cli`, not the
        # worker-side `caos`, in its devShell) — and it runs on the *host*. On
        # Linux the musl `caos-cli` already runs on the host, so copy it straight
        # out of the `caos` package; on macOS that's a Linux binary, so build a
        # native `caos-cli` for the host instead.
        nativeArgs = {
          inherit src;
          strictDeps = true;
          pname = "caos-host-tools";
          version = "0.1.0";
          # Dev profile here too (the macOS host build of caos-cli),
          # matching commonArgs — one profile everywhere.
          CARGO_PROFILE = "dev";
          CARGO_PROFILE_DEV_DEBUG = "line-tables-only";
        };
        nativeCliArtifacts = craneLib.buildDepsOnly (
          nativeArgs
          // { cargoExtraArgs = "--package caos --bin caos-cli"; }
        );
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
                cargoArtifacts = nativeCliArtifacts;
                cargoExtraArgs = "--package caos --bin caos-cli";
                pname = "caos-cli";
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
        # runnerd, brought up by `caosd up`. The network and container
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
              environment:
                # A fully cold suite (decomposed build members + image jobs +
                # per-test jobs) legitimately queues jobs behind the pool for
                # minutes; the claim timeout must mean "no runner exists",
                # not "runners are busy".
                CAOS_PENDING_TIMEOUT_SECS: "900"
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
          # Streamed to the registry by build-builtins.sh, never git-imported.
          workerFlakeBuilderImage
          workerRunnerImage
        ];

        # The worker binaries build-builtins.sh needs at publish — curried
        # onto std/runner (the agent harness, rustc, the example workers) and
        # published whole as refs/caos/bins (the build/test tools' input).
        # Handed over prebuilt so caosd needs no runtime nix. It finds each
        # binary under /bin, so these may share one consolidated output.
        builtinWorkerBins = [
          worker-bash-tool
          worker-llm-step
          worker-rgrep
          # Not curries: these ride only in refs/caos/bins — the test
          # suite's own image pipeline stages them (std's runner bakes its
          # /worker in workerRunnerImage; std/cargo compiles its own).
          worker-cargo
          worker-runner
          # Published as curry(runner, worker1) with the cargo ref and the
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
          # jq: build-builtins.sh parses the flake-builder tarball's image
          # manifest when streaming it to the registry. tar: it unpacks
          # that tarball for the same streaming step.
          # docker rides in from the host PATH (caosd already requires it).
          # util-linux: setsid, so a hung compose up dies as a whole group.
          runtimeInputs = [
            pkgs.coreutils pkgs.git pkgs.curl pkgs.bash pkgs.jq pkgs.gnutar
            pkgs.util-linux
          ];
          text = ''
            : "''${CAOS_DATA:=$PWD/.caos-data}"
            CAOS_DATA="$(readlink -m "$CAOS_DATA")"
            export CAOS_DATA
            mkdir -p "$CAOS_DATA"

            compose() { docker compose -f ${composeFile} "$@"; }

            # `compose up -d` can hang FOREVER: podman-compose implements
            # depends_on as `podman wait --condition=running <dep>`, which
            # never returns if that dep failed to start (e.g. a missing
            # bind-mount source) — it prints "Error: …" for the failed
            # start and presses on to hang on the dependents. So bring-up
            # runs in its own process group with its output watched: an
            # Error line kills it within a second (a deadline backstops
            # errorless hangs), and every failure dies loudly with each
            # container's state — State.Error carries the runtime's reason.
            compose_up() {
              local log; log=$(mktemp)
              setsid docker compose -f ${composeFile} up -d "$@" >"$log" 2>&1 &
              local pid=$! deadline=$((SECONDS + 120))
              fail() {
                echo "caosd: $1 — killing compose up" >&2
                kill -TERM -- "-$pid" 2>/dev/null || true
                cat "$log" >&2
                compose_up_diagnose
              }
              while kill -0 "$pid" 2>/dev/null; do
                grep -q "^Error" "$log" && fail "bring-up reported an error"
                [ "$SECONDS" -ge "$deadline" ] && fail "bring-up hung for 120s"
                sleep 1
              done
              cat "$log" >&2
              wait "$pid" || compose_up_diagnose
            }
            compose_up_diagnose() {
              echo "caosd: stack bring-up failed; container states:" >&2
              local c
              for c in caos-redis caos-registry caos-server caos-runnerd; do
                docker inspect \
                  -f "  $c: {{.State.Status}} {{.State.Error}}" "$c" >&2 \
                  2>/dev/null || echo "  $c: not created" >&2
              done
              exit 1
            }

            case "''${1:-up}" in
            up)
              # Install THIS deploy's client next to the stack state: the
              # client and the stack are one trust boundary (the old,
              # known-good caos), and consumers that build nothing — the
              # test runner — find it at $CAOS_DATA/bin/caos-cli.
              mkdir -p "$CAOS_DATA/bin"
              install -m 755 ${caos-cli}/bin/caos-cli "$CAOS_DATA/bin/caos-cli"
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
              # The bind-mount sources in the compose file above. podman-compose
              # creates a missing source only while CREATING a container
              # (os.makedirs in its mount processing); `up -d` on an existing
              # container just starts it, so a source deleted after creation
              # (a wiped CAOS_DATA) dies in crun. caosd owns this layout —
              # make it true before compose looks.
              mkdir -p "$CAOS_DATA/redis" "$CAOS_DATA/registry" \
                       "$CAOS_DATA/server-repo.git"

              echo "==> starting stack (redis, registry, server, runnerd)" >&2
              compose_up
              # Recreate exactly the services running a stale image — nothing else,
              # so an unchanged `up` (and a freshly-created stack) keeps its
              # containers in place.
              if [ "''${#stale[@]}" -gt 0 ]; then
                echo "==> recreating onto rebuilt image(s): ''${stale[*]}" >&2
                compose_up --force-recreate "''${stale[@]}"
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
      in
      {
        packages = {
          # `nix build` (no attr) yields the PINNED host tools: caos-cli,
          # caos, caosd, and (on Linux) caos-runnerd in one result/bin. This is the
          # explicit build step — its `caosd` bakes the exact server/runnerd/
          # worker image store paths from THIS checkout, so `result/bin/caosd up`
          # runs that fixed stack and never changes when you edit code. Rebuild
          # (another `nix build`) is the only thing that moves it. The workspace
          # binaries stay available as `.#caos`.
          default = caos-tools;
          inherit caos server runnerd caos-cli caosd caos-tools;
          # Agent-harness worker binaries (run as curry(runner, bin)) and the
          # llm-step tests' stub LLM server.
          inherit worker-bash-tool worker-llm-step worker-rgrep llm-stub;
          # The staged /worker binaries (std/runner, std/cargo) and the rustc
          # orchestrator (curry(runner, worker1)) — build-builtins.sh needs
          # the binaries exposed.
          inherit worker-runner worker-cargo worker-rustc;

          # The generated compose file, for driving the stack by hand
          # (`docker compose -f $(nix build --print-out-paths .#docker-compose)
          # up`). `caosd` is the batteries-included way.
          docker-compose = composeFile;

          # Image tarballs (build with `nix build`, then `docker load < result`).
          caos-server-docker = serverImage;
          caos-runnerd-docker = runnerdImage;
          caos-worker-cargo-deps-docker = cargoDepsImage;
          # skopeo, from OUR locked nixpkgs — the in-caos bake job pushes its
          # image to the registry with it (`nix shell path:<ws>#skopeo`), and
          # taking it from the flake keeps the job pure (a bare `nixpkgs#`
          # ref would float with the global registry).
          skopeo = linuxPkgs.skopeo;
          caos-worker-flake-builder-docker = workerFlakeBuilderImage;
          caos-worker-runner-docker = workerRunnerImage;

          # The flake-builder contract's two names (design/flake-images.md,
          # design/flake-deps-image.md): handed THIS tree, the builder builds
          # #caosImage — the test stack — and seeds itself from #depsImage
          # first. Nothing on the host's dev path builds these; `nix build`
          # still yields the pinned host tools.
          caosImage = testStackImage;
          depsImage = testStackDepsImage;
        };

        apps = {
          # Bring the whole stack up in the foreground (Ctrl-C tears it down).
          caosd = {
            type = "app";
            program = "${caosd}/bin/caosd";
          };

          # Build the image and load it into the local docker daemon in one go.
          load-caos-server = {
            type = "app";
            program = "${loadServer}/bin/load-caos-server";
          };
          load-caos-runnerd = {
            type = "app";
            program = "${loadRunnerd}/bin/load-caos-runnerd";
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

          doc = craneLib.cargoDoc (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoExtraArgs = "--locked --workspace";
              RUSTDOCFLAGS = "-D warnings";
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
            # NOTE: `caosd` is deliberately NOT on the dev-shell PATH. It used to
            # be a launcher that ran `nix run .#caosd` against the live tree, so
            # every `caosd up` silently rebuilt the image closure from whatever
            # was checked out. The pinned workflow is explicit instead: run
            # `nix build` to produce ./result/bin/caosd (baked to this checkout),
            # then `./result/bin/caosd up`. The stack only moves when you rebuild.
            # `fly` CLI: auth (`fly auth token`), org/region lookup, and operating
            # the fly backend (apps, machines, logs). caosd itself talks to the
            # Machines API + registry over HTTP and does not need this.
            pkgs.flyctl
          ];
        };
      }
    );
}

# drv-probe comment
