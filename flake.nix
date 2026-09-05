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

        # The revision this flake was built from, baked into the host commands
        # so a person can tell WHICH caos they are running.
        #
        # This exists because the failure mode is invisible: a devShell that
        # fails to build leaves direnv on the PREVIOUS environment ("Falling
        # back to previous environment!"), so `caosd` can be months older than
        # the `flake.lock` that names it — and the symptom is an error message
        # that reads exactly like a caos bug. It cost a session. `caosd version`
        # and the usage banners answer it in one line.
        #
        # `dirtyRev` when the tree has uncommitted changes, so a working build
        # says so rather than claiming the last commit. Injected at RUNTIME (see
        # `caos-cli`), never compiled in: a compile-time rev would re-key the
        # Rust workspace on every commit, so a docs-only commit would rebuild
        # everything.
        caosRev = self.rev or self.dirtyRev or "unknown";

        # The Linux system whose binaries the Docker images carry. We build for
        # the host's architecture (no arch-cross), so on Linux this is just the
        # host; on macOS it's the matching Linux system, whose general-purpose
        # packages (git, tar, the docker client in the server image) are
        # substituted prebuilt from the binary cache — no local Linux build, no VM.
        linuxSystem = if pkgs.stdenv.hostPlatform.isAarch64 then "aarch64-linux" else "x86_64-linux";
        linuxPkgs = import nixpkgs { system = linuxSystem; inherit overlays; };

        # Toolchain is pinned via ./rust-toolchain.toml + the flake.lock'd
        # rust-overlay revision, so every build uses the same compiler.
        #
        # `minimal` plus exactly the two extensions that do work: clippy and
        # rustfmt, for the checks below and for any caos-tool that runs them
        # in-worker. NOT the `default` profile — measured, its extras over
        # minimal are rust-std-aarch64-unknown-linux-musl (135 MiB, and this
        # flake deliberately never arch-crosses) and rust-src (51 MiB,
        # IDE-only — rust-toolchain.toml says as much), against 31 MiB for
        # clippy+rustfmt. The dev shell adds rust-src back, so nothing is lost
        # where it is wanted. Binaries are identical either way: rustc and the
        # musl std are the SAME component derivations in every profile, and the
        # aggregate is a link farm that cannot influence codegen.
        #
        # Parameterised on the package set because the cargo worker image is
        # Linux (linuxPkgs) while the host build is the host's. On Linux those
        # are one derivation, which is what collapses the bake and
        # cargoArtifacts into a single compile.
        rustChannel = (builtins.fromTOML (builtins.readFile ./rust/rust-toolchain.toml)).toolchain.channel;
        mkRustToolchain =
          p:
          (if rustChannel == "stable" then p.rust-bin.stable.latest else p.rust-bin.stable.${rustChannel})
          .minimal.override
            {
              extensions = [
                "clippy"
                "rustfmt"
              ];
              targets = [ muslTarget ];
            };
        rustToolchain = mkRustToolchain pkgs;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # The cargo source: ./rust, the whole of it. Everything cargo compiles
        # lives under that one directory — Cargo.toml, Cargo.lock,
        # rust-toolchain.toml and crates/ — which is what lets a package DECLARE
        # the workspace as a dependency (`../../rust rust` in a DEPS) rather
        # than be handed the repository.
        #
        # It is also what retired the exclusion that used to be here. Rooting at
        # the repo made `cleanCargoSource` sweep in every Cargo.toml in the
        # tree, so the suite's cargo FIXTURES — tests/cargo-check/{broken,mini},
        # tests/cargo-crates/ws — landed in the DEPENDENCY cache key and editing
        # one rebuilt all ~176 deps. They are runtime data handed to the cargo
        # worker as trees to check, never compiled by this build; now they are
        # simply not under `src`.
        #
        # NOT cleanCargoSource: crane's filter keeps *.rs, *.toml, Cargo.lock
        # and .cargo/config and NOTHING else, so a `include_str!("x.sh")`
        # compiles everywhere a full tree is present and fails HERE — which is
        # exactly how crates/worker-llm-step/src/githist/*.sh broke this build
        # after passing the whole suite (the suite compiles over the real
        # crates/, so the flake's filter is the one thing `run-tool test` never
        # exercises). Keep crates/**/*.sh: scripts a worker bakes into its
        # binary are source, not data.
        #
        # tests/lint/lint-flake-src.sh is the other half of this rule — it resolves every
        # include!/include_str!/include_bytes! under rust/crates and fails if the
        # target is not kept here, WITHOUT running nix, so the suite can hold
        # it. Widen this filter and you widen its keep_rule.
        #
        # This does not widen the DEPENDENCY key: buildDepsOnly builds
        # mkDummySrc, which keeps only Cargo.lock, .cargo/config.toml and
        # stubbed Cargo.tomls — never the .sh.
        src = pkgs.lib.cleanSourceWith {
          src = pkgs.lib.cleanSource ./rust;
          name = "source";
          filter =
            path: type:
            let
              rel = pkgs.lib.removePrefix (toString ./rust + "/") (toString path);
              isCrateScript = pkgs.lib.hasPrefix "crates/" rel && pkgs.lib.hasSuffix ".sh" rel;
            in
            craneLib.filterCargoSources path type || isCrateScript;
        };

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

        # A musl C cross-compiler: rustc links musl self-contained, but
        # C-carrying deps compile via cc-rs, which needs a real musl cc. It
        # must be set identically here and in the bake below — it is part of
        # the fingerprint, so a mismatch is the difference between one dep
        # build and two. (Same expression as std/cargo/bake.nix, evaluated
        # against each side's own package set: on Linux they are one
        # derivation, on macOS the image's is built Linux-hosted by the remote
        # builder, which is what BUILDING_ON_MACOS.md sets up.)
        muslCrossCC =
          if pkgs.stdenv.hostPlatform.isAarch64 then
            pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc
          else
            pkgs.pkgsCross.musl64.stdenv.cc;
        muslCCEnv = {
          "CC_${builtins.replaceStrings [ "-" ] [ "_" ] muslTarget}" =
            "${muslCrossCC}/bin/${muslCrossCC.targetPrefix}cc";
        };

        commonArgs = {
          inherit src;
          strictDeps = true;

          # Shared across deps + every crate so crane keys the dep cache the
          # same way every time.
          pname = "caos-workspace";
          version = "0.1.0";

          # These three exist to make cargoArtifacts and the cargo worker's
          # bake (std/cargo/bake.nix, called below with this same `src` and
          # toolchain) the SAME derivation. They used to differ only by these
          # and the toolchain profile — never by what was compiled — so the
          # same ~176 dependencies were built twice: once here for the
          # binaries, once inside caos for the worker image.
          cargoExtraArgs = "--locked --workspace";

          # The two absolute paths cargo fingerprints against. The worker must
          # materialize the workspace at exactly these locations: build-script
          # executables and their OUT_DIRs are keyed on the target dir too.
          postInstall = ''
            wsroot=$(pwd -P)
            echo -n "$wsroot" > $out/ws-root
            targetdir="''${CARGO_TARGET_DIR:-target}"
            case "$targetdir" in /*) ;; *) targetdir="$wsroot/$targetdir" ;; esac
            echo -n "$targetdir" > $out/target-dir
          '';

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
        // crossLinkerEnv
        // muslCCEnv;

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

        # The same workspace, stamped for the TEST world (crates/caos-world).
        # A dev stack is built from this; the host's is built from the above, so
        # a client of one is refused by the other's server.
        #
        # IT HAS TO BE A SECOND COMPILE, and that is not a design choice here:
        # the tag is `option_env!`, read by rustc, because caos-world explains
        # why it cannot be read at runtime — the interpreter exports its
        # environment into `worker1`, so an env-carried tag would travel with it
        # and declare the wrong binary correct. The tag is a property of the
        # ARTIFACT, so two worlds are two artifacts.
        #
        # WHAT IT DOES NOT DUPLICATE IS THE EXPENSIVE HALF. `cargoArtifacts` is
        # `buildDepsOnly commonArgs`, and this adds nothing to `commonArgs` — so
        # the ~176 dependencies are one derivation shared by both worlds, and
        # only the thin workspace compile happens twice. caos-tools/build's own
        # guard measured that split: the deps are 12.6s of a 15.0s cold build,
        # the workspace alone 2.4s.
        #
        # Do NOT move the stamp into `commonArgs` to "share more". It would land
        # in the dependency key and rebuild all of them per world, which is the
        # exact opposite of the intent.
        testWorkspaceBins = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            cargoExtraArgs = "--workspace";
            doCheck = false;
            CAOS_WORLD = "test";
          }
        );

        # Every crate's binary is selected (by name, at copy time) from the one
        # build above, so these are all the same derivation. The generic `caos`
        # worker and the user-facing `caos-cli` are separate Cargo
        # packages; consumers copy only the named binary they need.
        caos = workspaceBins;
        server = workspaceBins;
        runnerd = workspaceBins;
        worker-rustc = workspaceBins;
        worker-runner = workspaceBins;
        worker-cargo = workspaceBins;
        # The agent-harness workers (design/agent-harness.md). They have no
        # image of their own: each runs as curry(runner, bin=<static binary>)
        # in the shared runner pool, so only the binaries are exposed.
        worker-deep-deps = workspaceBins;

        # Minimal images: each contains *only* its static binary — no shell, no
        # libc, no /nix/store. Crates are unprefixed (caos, server) but
        # the published image names carry a `caos-` prefix.
        # The images are Linux, but build on macOS too (no VM): the Rust binaries
        # cross-compile for the host arch via rust-lld (see muslCrossLinker), and
        # the server image's tools come from linuxPkgs — substituted prebuilt from
        # the binary cache.

        # Two worker images live here — the host-built streamed core: the
        # flake-builder (below) and the runner (workerRunnerImage). Every
        # other std entry is a literal checked-in flake tree (std/bash —
        # design/flake-images.md part 2), each defining its
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
        # lock churn — passing OUR nixpkgs (tests/lint pins the
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
        # phases 0–1): ONE definition in std/cargo/bake.nix, called with THIS
        # flake's `src` and toolchain. That is what makes `cargoBake.deps` and
        # `cargoArtifacts` the same derivation instead of two compiles of the
        # same ~176 crates — the bake used to resolve its own `minimal`
        # toolchain, and cargo fingerprints on the exact compiler build.
        cargoBake = import ./std/cargo/bake.nix {
          pkgs = linuxPkgs;
          inherit crane src;
          toolchain = mkRustToolchain linuxPkgs;
        };

        # std/cargo as HOST-BUILT streamed core, like std/flake-builder and
        # std/runner: we call the subflake's outputs directly — the standard
        # subflake call, no path-input lock churn — passing OUR nixpkgs,
        # rust-overlay and crane, plus the workspace source and the shared
        # toolchain. Nothing resolves std/cargo as a flake TREE any more,
        # which is what let its vendored copies of the manifests, lockfile and
        # crate stubs go: a published tree must be self-contained, a streamed
        # image need not be.
        cargoDef =
          ((import ./std/cargo/flake.nix).outputs {
            self = null;
            inherit nixpkgs rust-overlay crane;
          }).lib.${linuxSystem}.imageFor
            {
              inherit src;
              toolchain = mkRustToolchain linuxPkgs;
            };
        workerCargoImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-cargo";
          tag = "latest";
          # CLEAN, exactly like the runner: no caos, no user db, no /tmp — and
          # no /worker. build-builtins composes the additions AND worker-cargo
          # on as content-keyed layers at publish. This image is 3.4 GB of
          # toolchain and baked deps; it must depend on nothing in the
          # workspace, or every Rust edit re-tars and re-gzips all of it and
          # then pushes it again.
          contents = cargoDef.contents;
          config = cargoDef.config;
          fakeRootCommands = cargoBake.inflateWith pkgs;
        };

        # ---- The stack root ----
        # Everything the host's stack image adds over its userland, which is
        # now just the one bring-up. It carries NO binaries: caosd stages
        # `server` and `runnerd` into the state mount at `up` (see
        # stage_bins), so this image — and therefore the `docker load` — is
        # source-independent. Baking them in instead made a 181 MB image
        # re-key on every Rust edit, so `nix build` re-tarred and re-gzipped
        # the whole userland and `caosd up` re-ran `docker load`, all to move
        # binaries the image shares with nothing.
        #
        # A TEST stack is not built here at all: caos-tools/build.sh assembles
        # it inside caos, from compiled binaries, as one layer over
        # `builderImage`. That is what lets the tree handed to the
        # flake-builder be the reduced one.
        hostStackRoot = pkgs.runCommand "caos-host-stack-root" { } ''
          # ONE SCRIPT, and it is the one that cannot come from anywhere else.
          # `bootstrap` is the entrypoint: it binds the store, builds the tree's
          # stack inputs and execs `<tree>/stack/serve` — so serve, the publish
          # and the daemons all come from the TREE, and editing any of them
          # needs no `docker load`. This file changes about as often as the
          # image's userland does, which is the property that keeps `nix build`
          # from re-tarring 160 MB on a source edit.
          #
          # STILL NO /caos/images AND NO /caos/tree. They existed so a stack
          # could run build-builtins.sh from inside itself, and were removed
          # because ~250 MB of tarballs and a copy of the tree in the image
          # re-keyed it constantly. Publishing from inside is back — but the
          # tree arrives at runtime through the state mount, and the tarballs
          # are built in here rather than carried in, so neither is baked.
          mkdir -p $out/caos/stack
          install -m 755 ${./stack/bootstrap} $out/caos/stack/bootstrap
        '';
        # The userland a caos stack shells out to, shared by both worlds.
        stackUserland = [
            # git (the server's
            # smart-HTTP transport and the publish client), skopeo + certs
            # (registry copies), redis (the private inner result cache), the
            # docker client (the inner runnerd delegating to the outer
            # engine), and a shell + the usual tools for the scripts that run
            # here.
            linuxPkgs.bashInteractive
            linuxPkgs.coreutils
            # cmp/diff: the tests compare cached results byte for byte, and
            # tests/lint's two lint scripts diff and grep. This
            # image is the environment every test runs in.
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
            # The image cache, a member of the group in whichever placement
            # owns one (stack/serve). Replaces compose's stock `registry:2`.
            linuxPkgs.distribution
            linuxPkgs.skopeo
            linuxPkgs.cacert
            # THE STACK BUILDS WHAT IT RUNS (stack/bootstrap): one `nix build`
            # of the tree's stack inputs, into a store volume shared with every
            # other caos container on this host. This is what lets `caosd`
            # carry a 4 MB tree instead of 1.6 GB of tarballs, and what takes
            # the three clean images out of the host's own `nix build`.
            # Measured: the flake-builder image is a complete nix image in
            # 99 MB, so this is not the expensive part of a 160 MB userland.
            linuxPkgs.nix
            # mount(8) for the bind that swaps that store over this image's
            # own, and flock(1) to serialise the copy that precedes it —
            # several containers seed one volume (see stack/bootstrap).
            linuxPkgs.util-linux
            (if pkgs.stdenv.hostPlatform.isLinux then
              linuxPkgs.docker-client.override { buildxSupport = false; composeSupport = false; }
            else
              linuxPkgs.docker-client)
        ];
        stackEnv = [
          "PATH=/bin"
          "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
          # Root: this image starts daemons, owns a git dir, and drives the
          # engine socket — the same per-image containment grant the
          # flake-builder carries.
          "CAOS_WORKER_UID=0"
          "CAOS_WORKER_GID=0"
          # The engine socket, declared the same way: this image hosts a caos
          # stack whose own runnerd launches siblings on the engine, so it
          # needs the socket runnerd was handing to EVERY worker before this
          # existed. Only an image can ask — the grant is read from the
          # image's config, never from a job or its args.
          "CAOS_GRANT_ENGINE_SOCKET=1"
          # Single-user, unsandboxed nix: root in a container with no nixbld
          # group and no privilege to build a sandbox. `stack/bootstrap` runs
          # one `nix build` here, and the same three settings the flake-builder
          # and dev/test-stack pass for the same situation — as NIX_CONFIG, so a
          # plain `nix build` inherits them.
          ''NIX_CONFIG=experimental-features = nix-command flakes
build-users-group =
sandbox = false''
          "NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
          # Explicitly, alongside the /etc/passwd entry that makes it
          # resolvable: git and skopeo both look HOME up, and the two answers
          # disagreeing is a worse failure than either being absent.
          "HOME=/root"
        ];
        # THE image this flake defines (`#caosImage`, the flake-builder
        # contract): the ENVIRONMENT a test stack is built and run in — never
        # the stack itself.
        #
        # It carries no workspace binaries, no seed, and no source, which is
        # the whole point: its inputs are the toolchain, the manifests and the
        # lockfile, so `caos-tools/build.sh` can hand the flake-builder a
        # REDUCED tree (flake + locks + manifests + zero-byte target stubs) and
        # get a registry hit on every source edit. Verified: the reduced tree
        # yields a byte-identical dep-bake derivation to the full tree, from
        # 220 KB instead of 164 files.
        #
        # That retires `#depsImage` entirely. The deps memo existed only to
        # carry a source-independent closure across a tree hash that moved on
        # every edit; with the tree hash held still, there is nothing to carry.
        #
        # Both halves ride here because the build stage does both jobs in this
        # one image: `cargoBake` to COMPILE the passed-in source, and
        # `stackUserland` because the image it assembles must run a stack (and
        # because cargo-check / cargo-self / rust-worker want a toolchain
        # anyway). The assembled stack image bases on this one and adds only
        # binaries, `serve`, `/worker` and the seed.
        # /worker: the std/bash contract verbatim — fetch `worker1` and run it
        # with bash. That is what lets caos-tools/build.sh be ONE staged
        # script: stage 1 reduces the tree and tail-calls the flake-builder,
        # stage 2 is `curry(<this image>, worker1=build.sh, stage=2, …)` and so
        # runs the SAME script with the toolchain and the baked deps in scope.
        # Copied rather than shared because std/bash is a published tree and
        # this is a nix-built image; tests/lint keeps the rules honest
        # elsewhere in the tree for the same reason.
        builderWorker = pkgs.runCommand "caos-builder-worker" { } ''
          mkdir -p $out
          install -m 755 ${./std/bash/worker} $out/worker
        '';
        # The three CLEAN core images, as tarballs at a fixed path. std names
        # them (runner, cargo, flake-builder), and the stage that publishes std
        # runs in THIS image — so unlike the old arrangement, where the stack
        # image carried them so a stack could publish from inside itself, they
        # belong to the builder and never reach the assembled stack image.
        #
        # Safe for the reduced-tree key precisely because all three are
        # source-independent: their /worker binaries ride as content-keyed
        # publish-time layers, not baked in. Adding anything workspace-derived
        # here would re-key the builder on every edit and undo the whole point.
        builderImages = pkgs.runCommand "caos-builder-images" { } ''
          mkdir -p $out/caos/images
          ${pkgs.lib.concatMapStringsSep "\n" (i: "cp ${i} $out/caos/images/$(basename ${i})") builtinWorkerImages}
        '';
        builderImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-test-stack-builder";
          tag = "latest";
          contents = [ builderWorker builderImages cargoBake.rootEnv ] ++ stackUserland;
          config = {
            Entrypoint = [ "/bin/caos" "runner" ];
            # cargoBake.env's PATH already ends in /bin (where the caos
            # additions land), so the stack-side vars append cleanly.
            Env = cargoBake.env ++ [
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              # Root, and the engine socket: the assembled stack image inherits
              # this config, and it hosts a stack whose runnerd launches
              # siblings. Only an image can ask for the socket.
              "CAOS_WORKER_UID=0"
              "CAOS_WORKER_GID=0"
              "CAOS_GRANT_ENGINE_SOCKET=1"
            ];
          };
          fakeRootCommands = cargoBake.inflateWith pkgs + ''
            ln -sf bash bin/sh
            # Workers scratch under /tmp; a bare nix root has none.
            mkdir -p tmp
            chmod 1777 tmp
          '';
        };
        # The same image in the `host` world, which `caosd up` runs directly —
        # so its entrypoint is the stack itself, not the runner protocol.
        hostStackImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-stack";
          tag = "latest";
          contents = [ hostStackRoot ] ++ stackUserland;
          config = {
            # bash EXPLICITLY, not the script's `#!/usr/bin/env bash` shebang:
            # /usr/bin/env comes from the caos additions, which the
            # flake-builder stacks onto WORKER images. caosd runs this one
            # directly, so nothing has stacked anything — and the failure is a
            # bare "No such file or directory" naming the script, not env.
            Entrypoint = [ "/bin/bash" "/caos/stack/bootstrap" ];
            Env = stackEnv;
          };
          # WHAT A BARE NIX ROOT DOES NOT HAVE, and now needs — because this
          # image both BUILDS (stack/bootstrap's `nix build`) and PUBLISHES
          # (serve's build-builtins) rather than being handed the results.
          # dev/test-stack's image carries the same lines, and its comment
          # records each as a distinct failure before it was a line:
          #
          #   /tmp, /var/tmp   a writable TMPDIR for every derivation nix
          #                    realises, and for the nix-DB dump bootstrap
          #                    takes before binding the store over /nix.
          #                    Absent, that bind fails with a bare
          #                    `/tmp/image-db.dump: No such file or directory`
          #                    and the stack silently comes up on the image's
          #                    own store, rebuilding everything every time.
          #   /etc/passwd,     skopeo resolves HOME through the user database
          #   /etc/group,      and dies `unable to resolve HOME directory:
          #   /root            user: unknown userid 0` without one. It only
          #                    surfaced when the publish moved in here: skopeo
          #                    used to run on the host, where root is real.
          fakeRootCommands = ''
            mkdir -p tmp var/tmp
            chmod 1777 tmp var/tmp
            mkdir -p root etc
            printf 'root:x:0:0:root:/root:/bin/bash\n' > etc/passwd
            printf 'root:x:0:\n' > etc/group
          '';
        };



        # ---- Cross-tree consumption: caos-cli, the stack, the stdlib ----
        # These let another tree (one that has caos as a flake input) get the
        # user-facing CLI on its PATH, bring the dev stack up, and publish the
        # builtin worker library — without the caos source tree.

        # Just the user-facing CLI (a consumer wants only `caos-cli`, not the
        # worker-side `caos`, in its devShell) — and it runs on the *host*. On
        # Linux the musl `caos-cli` already runs on the host, so copy it straight
        # out of the caos-cli package; on macOS that's a Linux
        # binary, so build a native `caos-cli` for the host instead.
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
          // { cargoExtraArgs = "--package caos-cli --bin caos-cli"; }
        );
        # Installed under both names: `caos` is what a person types (`caos talk`),
        # `caos-cli` stays for scripts and docs that spell it out. (No collision
        # with the worker-side `caos` binary — that one is baked into images and
        # never lands on a host PATH.)
        caos-cli-bin =
          if pkgs.stdenv.hostPlatform.isLinux then
            pkgs.runCommand "caos-cli-bin" { } ''
              mkdir -p $out/bin
              cp ${workspaceBins}/bin/caos-cli $out/bin/caos-cli
            ''
          else
            craneLib.buildPackage (
              nativeArgs
              // {
                cargoArtifacts = nativeCliArtifacts;
                cargoExtraArgs = "--package caos-cli --bin caos-cli";
                pname = "caos-cli-bin";
                doCheck = false;
              }
            );

        # The shipped command: the binary plus the revision it came from.
        #
        # Wrapped rather than compiled in, so the rev costs no rebuild — see
        # `caosRev`. One wrapper for both platforms, so macOS cannot drift into
        # printing a different answer.
        #
        # TWO wrappers, each with an explicit `--argv0`, rather than one plus a
        # `caos -> caos-cli` symlink: this `makeWrapper` emits a bare
        # `exec "<store path>" "$@"` with no `-a "$0"`, so a symlinked name is
        # lost and `prog_name` reports `caos-cli` however you typed it — every
        # usage line would then tell a person to run `caos-cli talk` when they
        # had just typed `caos`. `--argv0` restores the name each entry point is
        # meant to have. (`--set-default`, so a caller can still override the
        # rev, e.g. to reproduce what an older build printed.)
        caos-cli = pkgs.runCommand "caos-cli" { nativeBuildInputs = [ pkgs.makeWrapper ]; } ''
          mkdir -p $out/bin
          for name in caos-cli caos; do
            makeWrapper ${caos-cli-bin}/bin/caos-cli $out/bin/$name \
              --argv0 $name \
              --set-default CAOS_REV ${caosRev} \
              --run 'export CAOS_INVOKED_AS="$0"'
          done
        '';

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


        # The worker images that make up `std`, keyed by the same builtin name
        # build-builtins.sh maps them back to (via the <name> baked into each
        # tarball's store path). caosd hands these to build-builtins.sh so it
        # publishes the flake's own images without a runtime `nix build`.
        builtinWorkerImages = [
          # Streamed to the registry by build-builtins.sh, never git-imported.
          workerFlakeBuilderImage
          workerRunnerImage
          workerCargoImage
        ];

        # The worker binaries build-builtins.sh needs at publish, to curry onto
        # std/runner (rustc, deep-deps). Handed over prebuilt so caosd needs no
        # runtime nix. It finds each binary under /bin, so these may share one
        # consolidated output.
        builtinWorkerBins = [
          worker-deep-deps
          # Not curried, and not reached from here at all: the test suite's own
          # image pipeline stages these (std's runner bakes its /worker in
          # workerRunnerImage; std/cargo compiles its own). They are listed so
          # the deploy carries them, nothing more.
          worker-cargo
          worker-runner
          # Published as curry(runner, worker1) with the cargo ref and the
          # worker-common source curried in.
          worker-rustc
        ];

        # EVERYTHING A STACK BRING-UP NEEDS, in one derivation — so ONE
        # `nix build` produces all of it: the daemons under `bin/` and the
        # worker images under `images/`.
        #
        # `build-builtins.sh` takes both as store paths (CAOS_BUILTIN_BINS,
        # CAOS_BUILTIN_IMAGES) and runs no nix when it has them. Handing them
        # over is therefore the fast path, and this is what lets BOTH callers
        # take it: caosd for the host, caos-tools/test for a dev stack.
        #
        # On the host that aggregation used to fall out by accident — caosd's
        # script text names the image derivations, so building caosd built them.
        # The dev stack had no equivalent, so `dev/stack-up` evaluated the flake
        # three more times at runtime to find the same paths: 12 seconds of a
        # 16-second bring-up, against ~1 second to actually start the stack.
        #
        # `bin` is a symlink to the workspace's own, so a consumer looks for
        # `<inputs>/bin/<name>` exactly as it would in a plain build output. The
        # images keep their store basenames, which is how build-builtins maps
        # each back to its builtin (`*-caos-worker-<name>.tar.gz`).
        stackInputs =
          pname: bins:
          pkgs.runCommand pname { } ''
            mkdir -p $out/images
            ln -s ${bins}/bin $out/bin
            ${pkgs.lib.concatMapStringsSep "\n" (i: "ln -s ${i} $out/images/") builtinWorkerImages}
          '';

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
        # NO std-build AND NO std-check, and both went for the same reason.
        # Publishing is not separable from bring-up any more: `stack/serve`
        # publishes and then starts the seeder with the tree that publish
        # returned, because those two steps have to agree and the only way to
        # guarantee it is to make one a value the other consumes. So there is
        # nothing for `std-build` to do to a running stack — republishing is
        # `up`.
        #
        # `std-check` verified that the registry still held every image the SEED
        # RECORDS name. That divergence needed two things persisting
        # independently: `refs/caos/seed` in the git dir, and the blobs under
        # `stack/registry` — reachable because `image-cleanup` wipes the second
        # and deliberately not the first. The seed does not persist at all now;
        # it is recomputed on every bring-up, and build-builtins re-pushes any
        # clean image the registry is missing. The invariant it checked is a
        # property of how the seed is made, so there is nothing left to check.
        #   caosd image-cleanup  report rebuildable image-cache usage; with
        #                --execute, clear it while the stack is stopped. The
        #                next `up` republishes std and warms images on demand.
        #   caosd version  the caos revision THIS command was built from. Ask it
        #                before believing a bug report: a devShell that fails to
        #                build leaves direnv on the previous environment, so the
        #                `caosd` on PATH can be far older than the `flake.lock`
        #                that names it, and the symptom looks like a caos bug.
        #                `caos-cli`'s usage banner carries the same string.
        # `up` HANDS OVER A TREE, not build products: it copies the flake source
        # into the state mount and starts one container on `stack/bootstrap`,
        # which binds a persistent nix store, builds the stack's inputs inside,
        # and execs `stack/serve` to start the daemons, publish std and seed the
        # core from what that publish returned. So this command carries no
        # caos-cli, no worker images and no client repo, and runs no `nix`
        # itself — hence it still works from any directory, including a tree
        # that only imports this flake.
        # Uses the host's docker; CAOS_DATA (absolute) holds all persistent
        # state — server repo, the staged tree, publish client repo, redis,
        # registry.
        caosd = pkgs.writeShellApplication {
          name = "caosd";
          # COREUTILS AND BASH, AND THAT IS ALL. This command starts a container
          # and waits for a file; everything it used to shell out for now
          # happens inside the stack. skopeo + gzip were for pushing clean
          # images at std_build, git for walking the seed records at std_check,
          # curl for probing :9090, diffutils' cmp for deciding whether the
          # staged binaries had changed, util-linux's setsid for a compose
          # group that no longer exists — all of those callers are gone. Adding
          # one back is a sign that work has leaked out of the stack again.
          # docker rides in from the host PATH (caosd already requires it).
          runtimeInputs = [ pkgs.coreutils pkgs.bash ];
          text = ''
            : "''${CAOS_DATA:=$PWD/.caos-data}"
            CAOS_DATA="$(readlink -m "$CAOS_DATA")"
            export CAOS_DATA
            mkdir -p "$CAOS_DATA"

            NET=caos-net
            NAME=caos-stack
            REGISTRY_PORT=5000
            REGISTRY=localhost:$REGISTRY_PORT
            REGISTRY_REPO=$REGISTRY/caos

            # Load the stack image only when this exact build isn't already in
            # docker. The tag is content-addressed (sha1 of the image's
            # immutable store path), so an unchanged build skips the
            # multi-second `docker load`; a changed build has a new store path,
            # hence a new tag, and loads.
            load_once() {
              local name="$1" image="$2" src_tag old_tag repo_tag
              src_tag="$name-src:$(printf '%s' "$image" | sha1sum | cut -c1-12)"
              if docker image inspect "$src_tag" >/dev/null 2>&1; then
                echo "==> $name image already loaded — skipping docker load" >&2
              else
                echo "==> loading $name image into docker" >&2
                docker load -i "$image"
                docker tag "$name:latest" "$src_tag"
              fi
              # ALWAYS re-point `:latest`, which is the tag `docker run` names.
              # The sentinel says the image is PRESENT, never that `:latest`
              # still points at it: any other load of this repo moves that tag —
              # a second checkout, an older `caosd`, a `docker load` by hand —
              # and then the skip above runs the WRONG image, silently. That is
              # not hypothetical: it left a stack running a `serve` from before
              # the core-seeder-runner existed, so every seeded key fell through
              # to the generic runner and died pulling `seeded:latest`.
              docker tag "$src_tag" "$name:latest"

              # This function owns the content-addressed source tags. Retire
              # superseded ones here instead of rediscovering them at cleanup.
              #
              # SELECT THE REPO IN BASH, NOT IN THE ENGINE. `image ls <repo>`
              # matches IMAGES, and podman then prints every NAME of a matched
              # image — so asking for `<name>-src` also lists `<name>:latest`,
              # which is the SAME image under a second tag. This loop then
              # deleted the tag `docker run` names, and the very next command
              # (`docker image inspect <name>:latest`) died with "image not
              # known". Podman also spells a local repository `localhost/<name>`,
              # so a raw compare against $src_tag never matches even its own
              # tag — every run untagged everything it had just loaded.
              while IFS= read -r old_tag; do
                repo_tag=''${old_tag#localhost/}
                case "$repo_tag" in
                  "$name-src:"*) ;;
                  *) continue ;;
                esac
                if [ "$repo_tag" != "$src_tag" ]; then
                  docker image rm "$old_tag" >/dev/null 2>&1 || true
                fi
              done < <(docker image ls --format '{{.Repository}}:{{.Tag}}')
            }

            die() { # <message>
              echo "caosd: $1" >&2
              # 2>&1 THEN >&2: serve writes everything to stderr, so redirecting
              # docker's stderr to /dev/null would discard the entire diagnosis.
              docker logs "$NAME" 2>&1 >&2 || true
              exit 1
            }

            # NOTHING IS STAGED FROM THE STORE ANY MORE. This used to copy
            # `server`, `runnerd` and `core-seeder-runner` into /state/bin, and
            # to hand build-builtins the flake's prebuilt images and a client
            # repo — because the container had no nix and could build nothing.
            # It has nix now (stack/bootstrap), so the daemons, the clean
            # images and the publish client all come from one `nix build`
            # INSIDE, off a tree we copy in. What crosses the boundary is 4 MB
            # of source instead of 1.6 GB of tarballs, and `nix build` out here
            # no longer has to produce any of them.

            # ---- repack, and ONLY here ------------------------------------
            # A caos server repo accumulates one pack per push — a suite adds
            # hundreds — and gix's object store has a fixed 32 slots. At 33
            # packs the server starts answering `reading arg tree: object <oid>
            # not found: The slotmap turned out to be too small with 32
            # entries`, which reads as a missing object and is really a full
            # table. Measured, not predicted.
            #
            # WHY THIS IS THE ONLY SAFE PLACE. A repack REWRITES the object
            # store, and this repo's whole config exists to stop anything doing
            # that behind a live reader's back: `gc.auto 0`, `receive.autogc
            # false` and `maintenance.geometric-repack.enabled false` are three
            # settings with one job. A server holds its repo open as a
            # `gix::ThreadSafeRepository` for its lifetime, and when that
            # object database re-consolidates against packs that moved under
            # it, gix asserts and the request thread panics — the caller gets a
            # body-less 500 and the log says nothing (see the server config
            # above). `dev/stack-up` used to do this at every dev bring-up,
            # which was harmless while one stack existed at a time and became a
            # live peer's silent 500 as soon as two did.
            #
            # Here, nothing is reading: `up` has just removed the stack, and we
            # refuse if any worker is still alive. So this is the one moment a
            # rewrite is safe, and it covers BOTH repos — the host's own, and
            # the one every dev stack shares.
            #
            # `-a -d -k` and the `-k` is load-bearing: GC is off precisely
            # because almost every object here is unreachable from any ref
            # (SPEC), so a repack that dropped unreachable objects would delete
            # most of the store.
            #
            # IN A CONTAINER, for both, even though the host repo is a plain
            # bind. It keeps one mechanism rather than two, and it is the only
            # way to reach the dev repo at all — that one lives in a named
            # volume, which on macOS is inside the engine's VM and has no host
            # path. It also means this command needs no `git` of its own, which
            # is why it now carries almost no runtime inputs.
            DEV_VOLUME=caos-vol-caos-dev
            REPACK_THRESHOLD=16

            # Is anything alive that could be READING a server repo? Two
            # sources, and both are needed — the same pair `image-cleanup`
            # checks before it wipes:
            #
            #   the stack container   holds $CAOS_DATA/stack/git open. `up`
            #                         removes it just above, but this function
            #                         must not depend on its one caller having
            #                         done so.
            #   anything runnerd started   carries its owner label, and a dev
            #                         stack IS a worker, so that one filter
            #                         covers every holder of the dev repo.
            caos_containers_running() {
              local running
              running=$(
                if [ "$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null || true)" = true ]; then
                  echo "$NAME"
                fi
                docker ps --filter label=caos.runnerd.owner --format '{{.Names}}'
              )
              [ -n "$running" ]
            }

            # ONE container, and it decides per repo. The threshold is a bound
            # on ONE repository's slot table, so summing two repos' packs and
            # comparing that would repack a 1 GB store because the other one
            # grew. Counting inside also removes the window a count-then-repack
            # pair would leave between them.
            #
            # The threshold rides in as `$1` rather than being interpolated
            # into the script text: this is a nix string containing a shell
            # string containing a shell script, and that is two layers of
            # quoting too many to add a third.
            repack_repos() {
              # Skipped rather than fatal: a person running `up` while a suite
              # is in flight wants a stack, not homework, and the next idle
              # `up` will do it.
              if caos_containers_running; then
                echo "==> caos containers are still running — skipping the repack" >&2
                return 0
              fi
              docker run --rm --entrypoint /bin/bash \
                -v "$CAOS_DATA/stack:/host" -v "$DEV_VOLUME:/dev-stack" \
                caos-stack:latest -c '
                  limit=$1
                  for d in /host/git /dev-stack/git; do
                    [ -d "$d" ] || continue
                    packs=$(ls "$d"/objects/pack/*.pack 2>/dev/null | wc -l)
                    if [ "$packs" -gt "$limit" ]; then
                      echo "==> repacking $d ($packs packs)" >&2
                      git -C "$d" repack -adkq || exit 1
                    fi
                  done
                ' _ "$REPACK_THRESHOLD" \
                || die "repacking the server repos"
            }

            # The registry and Redis are caches: git holds the image inputs and
            # `up` republishes the irreducible std images. Clearing both avoids
            # retaining a cached result that names a registry digest just removed.
            # Require an idle stack instead of hiding stop/restart orchestration
            # and recovery inside a cleanup command.
            image_cleanup() {
              local execute=no arg registry_size=0 image_id running

              for arg in "$@"; do
                case "$arg" in
                  --execute) execute=yes ;;
                  -h|--help)
                    echo "usage: caosd image-cleanup [--execute]"
                    return
                    ;;
                  *)
                    echo "caosd image-cleanup: unknown argument '$arg'" >&2
                    return 2
                    ;;
                esac
              done

              if [ -d "$CAOS_DATA/stack/registry" ]; then
                registry_size=$(du -sh "$CAOS_DATA/stack/registry" | cut -f1)
              fi
              echo "caosd image-cleanup: registry $registry_size"
              docker image ls --format '  local image {{.ID}}  {{.Size}}' \
                "$REGISTRY_REPO" | sort -u
              if [ "$execute" != yes ]; then
                echo "caosd image-cleanup: dry run; run 'caosd down' then pass --execute"
                return
              fi

              running=$(
                if [ "$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null || true)" = true ]; then
                  echo "$NAME"
                fi
                # A dev stack is a WORKER now, so runnerd's own label covers it —
                # there is no separate caos.test-stack label to look for.
                docker ps --filter label=caos.runnerd.owner --format '{{.Names}}'
              )
              if [ -n "$running" ]; then
                echo "caosd image-cleanup: CAOS is still running:" >&2
                while IFS= read -r tag; do echo "  $tag" >&2; done <<< "$running"
                echo "run 'caosd down' and wait for active workers before cleanup" >&2
                return 1
              fi

              case "$CAOS_DATA" in
                /|"")
                  echo "caosd image-cleanup: refusing unsafe CAOS_DATA '$CAOS_DATA'" >&2
                  return 1
                  ;;
              esac
              rm -rf "$CAOS_DATA/stack/registry" "$CAOS_DATA/stack/redis"

              while IFS= read -r image_id; do
                [ -n "$image_id" ] || continue
                docker image rm "$image_id" >/dev/null 2>&1 || true
              done < <(docker image ls -q "$REGISTRY_REPO" | sort -u)

              echo "caosd image-cleanup: cleared the registry, Redis, and unused local CAOS images"
              echo "caosd image-cleanup: run 'caosd up' to republish std"
            }

            # The revision this caosd was built from, printed by `version` and
            # by every usage banner. A stale binary on PATH is otherwise
            # indistinguishable from a caos bug (see `caosRev`).
            CAOS_REV=${caosRev}

            usage() {
              echo "caosd ($CAOS_REV)"
              echo "usage: caosd [up|down|reset|logs|image-cleanup|version]"
            }

            case "''${1:-up}" in
            version)
              echo "caosd $CAOS_REV"
              exit 0
              ;;
            up)
              # ONE container runs the whole daemon group (design/
              # one-stack-image.md), and it BUILDS WHAT IT RUNS: its entrypoint
              # is `stack/bootstrap`, which binds a persistent nix store, does
              # one `nix build` of the tree's stack inputs, and execs
              # `stack/serve` — which starts the daemons, publishes std, and
              # starts the seeder with the tree that publish returned.
              #
              # SO THIS COMMAND CARRIES NO BUILD PRODUCTS. It used to substitute
              # `caos-stack-inputs` at flake time and hand them in, which meant
              # producing this very binary built all three clean images —
              # including the 1.5 GB cargo one — and then those 1.6 GB had to
              # cross into the container. Now the tree crosses instead: 4 MB.
              #
              # ALWAYS RECREATE. There is no longer anything to compare: the
              # daemons are built inside from a tree we copy in on every run, so
              # "did the binaries move" is a question this side cannot answer
              # and does not need to. A warm `up` costs a container restart and
              # a cache-hit nix build; the reuse logic it replaces (bins_staged,
              # image-id compare, start-the-stopped-one) existed only to dodge
              # that cost and got the stale-deploy cases wrong in the process.
              load_once caos-stack ${hostStackImage}
              docker network inspect "$NET" >/dev/null 2>&1 \
                || docker network create "$NET" >/dev/null
              docker rm -f "$NAME" >/dev/null 2>&1 || true

              repack_repos

              # THE TREE, copied rather than mounted: a bind's source is
              # resolved by the ENGINE, which on macOS is a VM that sees only
              # the paths it has been given, while $CAOS_DATA is already shared
              # with it — the same reason the binaries used to be staged here.
              # `cp -RL` because a nix store path is a symlink farm and the
              # container must get files (CLAUDE.md, "everything an image's
              # contents puts on disk is a SYMLINK").
              echo "==> staging the tree into $CAOS_DATA/stack/tree" >&2
              rm -rf "$CAOS_DATA/stack/tree.new"
              mkdir -p "$CAOS_DATA/stack"
              cp -RL ${self} "$CAOS_DATA/stack/tree.new"
              chmod -R u+w "$CAOS_DATA/stack/tree.new"
              rm -rf "$CAOS_DATA/stack/tree"
              mv "$CAOS_DATA/stack/tree.new" "$CAOS_DATA/stack/tree"

              echo "==> starting the stack (redis, registry, server, runnerd)" >&2
              # Three aliases, one container: every name that resolved under
              # compose still resolves — workers reach the server as
              # `caos-server`, and the server pulls a delta's `base` as
              # `caos-registry:5000` (design/one-stack-image.md, the netns
              # section). Workers get their OWN netns on this bridge, which
              # is what lets a test stack bind its own :80 — and is also why
              # `caos-redis` has to exist as a NAME: a worker here shares no
              # loopback with the stack, so the address the server uses
              # (127.0.0.1) is meaningless to it.
              #
              # SYS_ADMIN and the store volume are for the bind in bootstrap
              # (and apparmor=unconfined with it: Ubuntu's docker-default
              # profile denies the mount regardless of the capability, as
              # runnerd notes for the worker's bind):
              # a volume cannot be mounted AT /nix, because this image's own
              # userland lives there and would be shadowed. Not a widening —
              # this container already holds the engine socket, which is
              # root-equivalent over the engine.
              docker run -d --name "$NAME" \
                --network "$NET" \
                --network-alias caos-server \
                --network-alias caos-registry \
                --network-alias caos-redis \
                --cap-add SYS_ADMIN \
                --security-opt apparmor=unconfined \
                -p 9090:80 -p "$REGISTRY_PORT:5000" \
                -v "$CAOS_DATA/stack:/state" \
                -v caos-vol-mounted-nix:/mounted-nix \
                -v /var/run/docker.sock:/var/run/docker.sock \
                -e CAOS_STACK_TREE=/state/tree \
                -e CAOS_STACK_INPUTS_ATTR=caos-stack-inputs \
                -e CAOS_STACK_STORE=/mounted-nix \
                -e CAOS_STACK_STATE=/state \
                -e CAOS_STACK_LOGS=/state/logs \
                -e CAOS_STACK_READY=/state/stack.ready \
                -e CAOS_CLIENT_REPO=/state/publish-client-repo \
                -e CAOS_REGISTRY_HTTP=localhost:5000 \
                -e CAOS_STACK_REDIS_PORT=6379 \
                -e CAOS_STACK_REDIS_PERSIST=yes \
                -e CAOS_STACK_REGISTRY=yes \
                -e CAOS_STACK_REDIS=yes \
                -e CAOS_STACK_RUNNERD=yes \
                -e CAOS_STACK_SEEDER=yes \
                -e CAOS_STACK_RUNNER_SERVER_URL=http://caos-server \
                -e CAOS_STACK_RUNNER_REDIS_ADDR=caos-redis:6379 \
                -e CAOS_REGISTRY_PULL_HOST="$REGISTRY" \
                -e CAOS_DOCKER_NETWORK="$NET" \
                -e CAOS_RUNNER_SOCKET=/var/run/docker.sock \
                -e CAOS_PENDING_TIMEOUT_SECS=900 \
                caos-stack:latest >/dev/null

              # WAIT FOR THE READY FILE, not for :9090. The server answering is
              # no longer the end of bring-up — std still has to be published
              # and the seeder started from it — and serve touches this only
              # once all of that is done. Waiting on the port instead would
              # return a stack whose core has no answerer.
              echo "==> waiting for the stack (build, publish, seed) ..." >&2
              rm -f "$CAOS_DATA/stack/stack.ready"
              ok=""
              for _ in $(seq 1 1800); do
                if [ -e "$CAOS_DATA/stack/stack.ready" ]; then
                  ok=1
                  break
                fi
                # serve dies as a group, so the container exiting IS the
                # diagnosis — don't wait out the full timeout for it.
                docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null \
                  | grep -q true || die "the stack exited during bring-up"
                sleep 1
              done
              [ -n "$ok" ] || die "the stack never finished bring-up"

              echo "==> stack up. 'caosd logs' to follow, 'caosd down' to stop." >&2
              ;;
            down)
              echo "==> stopping stack (CAOS_DATA kept; 'caosd reset' wipes it)" >&2
              docker rm -f "$NAME" >/dev/null 2>&1 || true
              ;;
            reset)
              echo "==> stopping stack and wiping CAOS_DATA state" >&2
              docker rm -f "$NAME" >/dev/null 2>&1 || true
              rm -rf "$CAOS_DATA/stack" "$CAOS_DATA/publish-client-repo"
              ;;
            logs)
              # The group's members log to files (one container, four daemons —
              # `docker logs` would interleave them); serve's own output is the
              # container's.
              tail -n +1 -f "$CAOS_DATA"/stack/logs/*.log
              ;;
            image-cleanup)
              shift
              image_cleanup "$@"
              ;;
            *)
              echo "caosd: unknown command '$1'" >&2
              usage >&2
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
          # The workspace stamped for the TEST world — what dev/stack-up builds
          # a dev stack from, so a host client cannot drive it (and vice versa).
          caos-test-world = testWorkspaceBins;
          # The two stack-bring-up aggregates (see `stackInputs`): one nix
          # build each, and both are handed to the same build-builtins.
          caos-stack-inputs = stackInputs "caos-stack-inputs" workspaceBins;
          caos-test-stack-inputs = stackInputs "caos-test-stack-inputs" testWorkspaceBins;
          # Agent-harness worker binaries (run as curry(runner, bin)).
          inherit worker-deep-deps;
          # The staged /worker binaries (std/runner, std/cargo) and the rustc
          # orchestrator (curry(runner, worker1)) — build-builtins.sh needs
          # the binaries exposed.
          inherit worker-runner worker-cargo worker-rustc;

          # Image tarballs (build with `nix build`, then `docker load < result`).
          caos-worker-flake-builder-docker = workerFlakeBuilderImage;
          caos-worker-runner-docker = workerRunnerImage;
          # The cargo worker, host-built and streamed like the flake-builder
          # and the runner: std/cargo is no longer a published flake tree, so
          # its image comes from here (see the cargoDef call above).
          caos-worker-cargo-docker = workerCargoImage;

          # The flake-builder contract (design/flake-images.md): handed a tree,
          # the builder builds `#caosImage`. What this flake defines under that
          # name is now the BUILD ENVIRONMENT, not the test stack — the stack
          # is assembled from compiled binaries by caos-tools/build.sh, which
          # is what lets the tree handed here be the reduced one.
          #
          # `#depsImage` is GONE, not renamed. It carried a source-independent
          # closure across a tree hash that moved on every source edit; the
          # reduced tree holds that hash still, so there is nothing left to
          # carry. (design/flake-deps-image.md is retired with it.)
          caosImage = builderImage;
        };

        apps = {
          # Bring the whole stack up in the foreground (Ctrl-C tears it down).
          caosd = {
            type = "app";
            program = "${caosd}/bin/caosd";
          };

        };

        # No `checks` output at all. test, clippy, doc and fmt all run in
        # tests/unit-{test,clippy,doc,fmt}, through the cargo worker, and
        # `caos-cli run-tool test` is the only runner anyone invokes —
        # there is no CI here, and
        # CLAUDE.md's pre-commit step is nix build + caosd up + run-tool test.
        #
        # Every one of them had a reason to move, and the tests had the
        # sharpest: git_transport_tests and chat::tests spawn git, which the
        # worker's PATH carries (bake.env's gitMinimal) and a nix builder's
        # does not, so that check had been silently red — on origin/main and
        # before a56df5a. `doc` was red too, on two rustdoc links. A check
        # nobody runs is a check that goes quietly red; one runner, and it is
        # the one with the environment the work needs.

        devShells.default = craneLib.devShell {
          # Brings the pinned toolchain (rustc, cargo, clippy, rustfmt) onto PATH.
          packages = [
            pkgs.cargo-watch
            pkgs.rust-analyzer
            # rust-src is IDE-only (stdlib source for navigation), so it rides
            # here rather than in the build toolchain — where it would land in
            # every worker image for 51 MiB of nothing.
            pkgs.rust-bin.stable.latest.rust-src
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
            pkgs.jless
          ];
        };
      }
    );
}
