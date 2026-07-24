{
  # std/cargo (design/flake-images.md): the whole-tree cargo worker — check/
  # build/test over a workspace — as ONE self-defining worker image: the
  # pinned rust toolchain + the caos workspace's dependencies pre-compiled
  # for (musl, dev), with /worker = the worker-cargo binary (copied into
  # this tree at publish by stage-tree.sh). rustc borrows this entry (its
  # curried `cargo` ref) for its compile step.
  #
  # The bake machinery lives in ./bake.nix, which the ROOT flake also
  # imports (for cargoDepsImage, the test suite's deps-only base) — one
  # definition, no drift. This file wraps the bake + /worker into the
  # #caosImage contract: a flake defines everything about the image except
  # the caos additions.
  #
  # The tree this flake ships in is GENERATED at publish by stage-tree.sh
  # (build-builtins.sh): these nix files + a flake.lock DERIVED from the
  # main flake.lock + the workspace's rust-toolchain.toml,
  # Cargo.toml/Cargo.lock and member manifests + the worker binary — no
  # source, so a source edit never re-keys the tree (registry memo
  # flake-<H>); a worker-cargo/worker_common edit does, and pays one cold
  # rebake. Deriving the lock pins this flake's rust-overlay/nixpkgs/crane
  # to the main flake's revisions: the toolchain resolves the same rustc the
  # caos build uses, which the caos-in-caos suite (a cargo worker building
  # caos itself) depends on.
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
          workerRoot = pkgs.runCommand "cargo-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${./worker} $out/worker
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
