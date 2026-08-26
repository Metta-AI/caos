{
  # dev/worker-test: the image a WORKER TEST runs in when std/bash is not
  # enough. `./worker` is a plain script interpreter — read it first, it says
  # what this is for and how it differs from dev/cli-test.
  #
  # The short version: same tools as dev/cli-test, no client and no repo. A
  # test names this when it needs `git` (the one thing caos has no verb for:
  # refs on the server) or another binary std/bash omits, and still wants to be
  # a worker rather than a client.
  #
  # THE IMAGE AND THE SCRIPT LIVE TOGETHER, deliberately, exactly as in
  # dev/cli-test: what a test can reach for and what the setup does with it are
  # one decision.
  #
  # WHAT IT DELIBERATELY DOES NOT CARRY: any grant. dev/test-stack declares
  # persistent volumes and CAP_SYS_ADMIN because it hosts a stack; a test does
  # not, and runnerd honours grants per image — so running tests in that image
  # would hand every one of them write access to the dev stack's own nix store
  # and object database.
  #
  # Nor a caos client, which is the whole reason this image exists.
  #
  # This directory IS the published tree (literal trees): flake.nix, worker.
  # The lock is NOT here: it is DEPped from the repo root and placed by the
  # flake-builder, so there is one lock in the tree rather than a copy per flake.
  description = "caos dev/worker-test — the image a WORKER test runs in when std/bash is not enough: git and friends, no client, no repo";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          workerRoot = pkgs.runCommand "worker-test-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${./worker} $out/worker
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "caos-worker-test";
          tag = "latest";
          # No Entrypoint: runnerd forces `/bin/caos runner`, which execs
          # /worker — which is the script interpreter.
          contents = [
            workerRoot
            pkgs.bashInteractive
            pkgs.coreutils
            # The reason this image exists: refs on the server.
            pkgs.gitMinimal
            pkgs.gnugrep
            pkgs.gnused
            pkgs.gawk
            pkgs.findutils
            # cmp/diff: several tests compare results byte for byte.
            pkgs.diffutils
            pkgs.gnutar
            pkgs.gzip
            pkgs.jq
            pkgs.curl
            pkgs.cacert
          ];
          config = {
            Env = [
              "PATH=/bin"
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
            ];
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
