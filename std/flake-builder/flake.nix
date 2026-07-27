{
  # std/flake-builder (design/flake-images.md): the image that builds flake
  # images — self-contained, defined like every other std entry. #caosImage
  # is the CLEAN image per the contract: a flake defines everything about
  # the image except the caos additions, /worker (the stage script)
  # included. Resolution never builds this flake — it IS the flake path, so
  # the host builds it (the root flake imports ./image.nix, adds the caos
  # additions, and build-builtins.sh streams it); this flake exists so the
  # entry's definition carries the same notation as everything else.
  #
  # The tree is LITERAL: this file, ./image.nix, ./worker, and a flake.lock
  # derived from the main flake.lock (std/refresh.sh; verified by
  # tests/std-lint).
  description = "caos std/flake-builder — builds flake images into runnable images";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          def = import ./image.nix { inherit pkgs; };
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "flake-builder";
          tag = "latest";
          contents = [ def.root ];
          config = def.config;
          # An in-image `nix build` needs the closure registered in the
          # store db, not just present under /nix/store.
          includeNixDB = true;
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
