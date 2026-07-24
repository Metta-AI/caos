{
  # std/cargo-base as a FLAKE (design/flake-images.md, finding B): the pinned
  # rust toolchain + the caos workspace's dependencies pre-compiled for
  # (musl, dev). The flake-builder turns this tree into the clean base image
  # `std/cargo` and `std/rustc` curry their worker binaries onto; /worker and
  # caos arrive via the builder's caos delta, never here — so this image is
  # keyed on (toolchain, manifests, lockfile) alone and a caos source edit
  # re-ships one small binary blob, not these layers.
  #
  # All the machinery lives in ./bake.nix, which the ROOT flake also imports
  # (for cargoDepsImage, the test suite's deps-only base) — one definition,
  # no drift. This file only wraps the bake into the #caosImage contract.
  #
  # The tree this flake ships in is GENERATED at publish by stage-tree.sh
  # (build-builtins.sh): these nix files + a flake.lock DERIVED from the main
  # flake.lock + the workspace's rust-toolchain.toml, Cargo.toml/Cargo.lock and
  # member manifests — no source, so a source edit never re-keys the tree
  # (registry memo flake-<H>). Deriving the lock pins this flake's
  # rust-overlay/nixpkgs/crane to the main flake's revisions: the toolchain
  # resolves the same rustc the caos build uses, which the caos-in-caos
  # suite (a cargo worker building caos itself) depends on.
  description = "caos std/cargo-base — pinned toolchain + baked workspace deps (musl, dev)";

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
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "cargo-base";
          tag = "latest";
          contents = [ bake.rootEnv ];
          config = {
            # No Entrypoint, no /bin content: the flake-builder forces the
            # runner entrypoint, and the /bin on bake.env's PATH is the caos
            # its delta stacks in.
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
