{
  # std/bash-base (design/flake-images.md): the script worker's environment —
  # a shell and the file tools a worker script leans on. A pure BASE: no
  # /worker, no caos; the flake-builder's delta supplies the setuid caos, the
  # runner trampoline, /tmp and the user db. std/bash is
  # curry(<this tree>, bin=images/bash-worker.sh) — the bin fetches the run's
  # `script` arg and executes it (build-builtins.sh).
  #
  # The published tree is this file + a flake.lock derived from the main
  # flake.lock at publish (stage-tree.sh), so the nixpkgs pin cannot drift
  # from the root flake's.
  description = "caos std/bash-base — shell + file tools for script workers";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "bash-base";
          tag = "latest";
          # bash provides /bin/sh too. No Entrypoint: the flake-builder forces
          # the runner entrypoint, and appends the delta's :/bin to PATH.
          contents = [
            pkgs.bash
            pkgs.coreutils
            pkgs.diffutils
            pkgs.gnugrep
            pkgs.findutils
          ];
          config = {
            Env = [ "PATH=/bin" ];
          };
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
