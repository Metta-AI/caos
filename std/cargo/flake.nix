{
  # std/cargo (design/flake-images.md): the whole-tree cargo worker — check/
  # build/test over a workspace — as ONE self-defining worker image: the
  # pinned rust toolchain + the caos workspace's dependencies pre-compiled
  # for (musl, dev), with /worker = the worker-cargo binary, built HERE from
  # the vendored source below. rustc borrows this entry (its curried `cargo`
  # ref) for its compile step.
  #
  # The bake machinery lives in ./bake.nix, which the ROOT flake also
  # imports (for cargoDepsImage, the test suite's deps-only base) — one
  # definition, no drift. This file wraps the bake + /worker into the
  # #caosImage contract: a flake defines everything about the image except
  # the caos additions.
  #
  # This tree is LITERAL (part 2): everything checked in, maintained by
  # std/refresh.sh and verified by tests/std-lint — this flake, its lock
  # (derived from the main flake.lock), the workspace's
  # rust-toolchain.toml/Cargo.toml/Cargo.lock, member manifests with EMPTY
  # target stubs, and REAL source for exactly two crates: worker-cargo (the
  # /worker) and worker-common (its path dep). Other crates ride as stubs,
  # so their source edits never re-key this tree; a worker-cargo or
  # worker-common edit does, and pays one cold rebake — same cadence as
  # when the binary was staged. Deriving the lock pins this flake's
  # rust-overlay/nixpkgs/crane to the main flake's revisions: the toolchain
  # resolves the same rustc the caos build uses, which the caos-in-caos
  # suite (a cargo worker building caos itself) depends on.
  description = "caos std/cargo — the cargo worker: pinned toolchain + baked deps + worker-cargo";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      crane,
    }:
    let
      # The flake-builder builds for its own (Linux) architecture; carry both.
      forSystem =
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };
          bake = import ./bake.nix {
            inherit pkgs crane;
            src = ./.;
            toolchainFile = ./rust-toolchain.toml;
          };
          # /worker, compiled from the vendored source against the bake's
          # own artifacts — same toolchain, target, profile, and env as the
          # bake, so the dep graph is a pure reuse, and the compile covers
          # only worker-cargo + worker-common.
          workerCargo = bake.craneLib.buildPackage ({
            src = ./.;
            pname = "worker-cargo";
            version = "0.1.0";
            strictDeps = true;
            cargoVendorDir = bake.vendor;
            cargoArtifacts = bake.deps;
            CARGO_PROFILE = "dev";
            CARGO_PROFILE_DEV_DEBUG = "line-tables-only";
            CARGO_BUILD_TARGET = bake.muslTarget;
            cargoExtraArgs = "--locked -p worker-cargo";
            doCheck = false;
          }
          // {
            ${bake.muslCCEnvName} = "${bake.muslCrossCC}/bin/${bake.muslCrossCC.targetPrefix}cc";
          });
          workerRoot = pkgs.runCommand "cargo-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${workerCargo}/bin/worker-cargo $out/worker
          '';

          # The deps memo (design/flake-deps-image.md). The expensive half of
          # this flake is source-INDEPENDENT — bake.deps is buildDepsOnly over
          # crane's dummy sources — so a memo keyed on its derivation survives
          # the worker-cargo / worker-common edits that re-key this tree.
          #
          # NOT a runtime environment: what belongs here is derivation
          # OUTPUTS, since nix skips a derivation iff its output path is valid
          # in the store. The registration rides as a plain file (an
          # includeNixDB db.sqlite would replace the consuming store's rather
          # than merge into it) and the builder merges it with
          # `nix-store --load-db`.
          depsRoots = [
            bake.deps
            bake.vendor
            bake.toolchain
            bake.muslCrossCC
            pkgs.stdenv.cc
          ];
          depsRegistration = pkgs.runCommand "cargo-deps-registration" { } ''
            mkdir -p $out
            cp ${pkgs.closureInfo { rootPaths = depsRoots; }}/registration \
              $out/caos-deps-registration
          '';
        in
        {
          caosImage = pkgs.dockerTools.buildLayeredImage {
            name = "cargo";
            tag = "latest";
            contents = [
              workerRoot
              bake.rootEnv
            ];
            config = {
              # No Entrypoint: runnerd forces `/bin/caos runner`, which execs
              # /worker. The /bin on bake.env's PATH is the caos the delta
              # stacks in.
              Env = bake.env;
            };
            fakeRootCommands = bake.inflate;
          };

          # Not runnable, by design: no entrypoint, no /worker, no caos, no
          # setuid. The flake-builder unpacks it into its own store and never
          # starts a container from it — which is what keeps every caos
          # version out of this memo's key.
          depsImage = pkgs.dockerTools.buildLayeredImage {
            name = "cargo-deps";
            tag = "latest";
            contents = [ depsRegistration ];
          };
        };
    in
    {
      packages = builtins.listToAttrs (
        map
          (system: {
            name = system;
            value = forSystem system;
          })
          [
            "x86_64-linux"
            "aarch64-linux"
          ]
      );
    };
}
