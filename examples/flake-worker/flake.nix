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
      # curried `bin` (here greet.sh, exec'd by the trampoline). So the base
      # carries only the tools the bin needs — no caos code, stable hash. bash
      # provides /bin/sh, so a shell-script bin runs.
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
