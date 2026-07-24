{
  description = "A trivial caos flake worker — validates the flake-image path.";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
    in
    {
      # The contract the flake-builder builds (design/flake-images.md). A
      # #caosImage is a pure BASE — no /worker: the flake-builder's caos delta
      # supplies the runner trampoline, and a worker is run as this image with a
      # curried `bin` (../greet.sh, exec'd by the trampoline). So the base
      # carries only the tools the bin needs — no caos code, stable hash. bash
      # provides /bin/sh, so a shell-script bin runs.
      #
      # The flake lives in env/ — its OWN directory — and the bin lives beside
      # it, deliberately: the flake tree is the image's cache key (the registry
      # memo flake-<treehash>), so only what the image build reads belongs in
      # it. A bin inside this tree would re-key (and pointlessly rebuild) the
      # image on every script edit. Same rule as the std entries' generated
      # trees (std/*/stage-tree.sh). Run it as:
      #   caos run examples/flake-worker/env -- --bin:@=examples/flake-worker/greet.sh
      packages.${system}.caosImage = pkgs.dockerTools.buildLayeredImage {
        name = "flake-worker";
        tag = "latest";
        contents = [
          pkgs.bash
          pkgs.coreutils
        ];
      };
    };
}
