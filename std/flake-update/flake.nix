{
  # std/flake-update: `nix flake update`, as a tool. The image is nix and what
  # its fetchers need (CA certs for github: inputs, git for git+ ones), with
  # /worker the script that re-locks the flake handed to it (./worker). Built
  # by the flake-builder like std/bash; the lock is DEPped from the repo root.
  #
  # The contract (std/flake-builder/worker): a flake defines everything about
  # the image except the caos additions. /worker included.
  description = "caos std/flake-update — re-lock a flake by running nix itself";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          script = pkgs.writeTextFile {
            name = "caos-worker-flake-update-script";
            executable = true;
            destination = "/worker";
            text = builtins.readFile ./worker;
          };
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "flake-update";
          tag = "latest";
          # No Entrypoint: runnerd forces `/bin/caos runner`, which execs /worker.
          contents = [
            script
            pkgs.nix
            # /etc/ssl/certs/ca-bundle.crt, for nix's https fetchers.
            pkgs.cacert
            pkgs.bash
            pkgs.coreutils
            # The worker reads the input's `original` out of flake.lock.
            pkgs.jq
            # git+ inputs are fetched with git; github: ones as tarballs.
            pkgs.git
          ];
          config = {
            Env = [
              "PATH=/bin"
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              "NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              # Root: nix writes the sources it fetches into the root-owned
              # store (the same grant std/flake-builder carries).
              "CAOS_WORKER_UID=0"
              "CAOS_WORKER_GID=0"
            ];
          };
          # nix needs its closure REGISTERED in /nix/var/nix/db, not merely
          # present in /nix/store.
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
