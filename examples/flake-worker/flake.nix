{
  description = "A trivial caos flake worker — a complete, self-contained worker image.";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
    in
    {
      # The contract the flake-builder builds (design/flake-images.md): a
      # #caosImage defines EVERYTHING about the image except the caos
      # additions (setuid /bin/caos, /tmp, the user db, /usr/bin/env) —
      # /worker included. This flake's /worker is its own script, so this is
      # a complete worker image: `caos run examples/flake-worker` and done.
      #
      # This tree is the image's cache key, so editing worker.sh rebuilds the
      # (trivial) image — the right trade for a self-contained worker. A
      # frequently-edited worker rides an interpreter image instead:
      # curry(std/runner, worker1=<binary>) or curry(std/bash, worker1=<script>).
      packages.${system}.caosImage = pkgs.dockerTools.buildLayeredImage {
        name = "flake-worker";
        tag = "latest";
        # bash provides /bin/sh, so the shell-script /worker runs.
        contents = [
          (pkgs.runCommand "flake-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${./worker.sh} $out/worker
          '')
          pkgs.bash
          pkgs.coreutils
        ];
      };
    };
}
