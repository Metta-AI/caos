{
  # Source-merge presentation around the server's object-only Git operation.
  description = "caos source merge worker";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          workerRoot = pkgs.runCommand "merge-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${./worker} $out/worker
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "merge";
          tag = "latest";
          # bash provides /bin/sh too. No Entrypoint: runnerd forces
          # `/bin/caos runner`, which execs /worker.
          contents = [
            workerRoot
            pkgs.bash
            pkgs.coreutils
            pkgs.jq
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
