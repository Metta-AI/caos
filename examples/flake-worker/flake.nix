{
  description = "A trivial caos flake worker — validates the flake-image path.";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      # A self-contained /worker (no bin curried in): caos runner execs it, it
      # writes a greeting and stores it as the job's result via the stacked
      # /bin/caos (the flake-builder's caos runner layer provides that).
      worker = pkgs.writeTextFile {
        name = "flake-worker-script";
        executable = true;
        destination = "/worker";
        text = ''
          #!${pkgs.bash}/bin/bash
          echo "hello from a flake-built worker" > /tmp/out
          /bin/caos put /tmp/out /cas/out
        '';
      };
    in
    {
      # The contract the flake-builder builds (design/flake-images.md).
      packages.${system}.caosImage = pkgs.dockerTools.buildLayeredImage {
        name = "flake-worker";
        tag = "latest";
        contents = [
          worker
          pkgs.bash
          pkgs.coreutils
        ];
      };
    };
}
