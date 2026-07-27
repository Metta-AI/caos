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
        in
        pkgs.dockerTools.buildLayeredImage {
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
    in
    {
      packages = builtins.listToAttrs (
        map
          (system: {
            name = system;
            value = {
              caosImage = forSystem system;
            };
          })
          [
            "x86_64-linux"
            "aarch64-linux"
          ]
      );
    };
}
