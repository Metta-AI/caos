{
  # std/flake-builder (design/flake-images.md): the image that builds flake
  # images — self-contained, defined like every other std entry. #caosImage
  # is the CLEAN image per the contract: a flake defines everything about
  # the image except the caos additions, /worker (the stage script)
  # included. Resolution never builds this flake — it IS the flake path, so
  # the host builds it: the ROOT flake calls this file's outputs and takes
  # #caosImage as-is, and build-builtins.sh streams it FROM scratch with
  # the caos additions composed on top as a content-keyed layer (the same
  # clean-image + additions-delta shape the stack stage gives every flake
  # image).
  #
  # The image is SELF-CONTAINED: nix, its fetchers' CA certs, the /worker
  # stage script, and the tools it runs, all from this nixpkgs. The worker
  # script needs no /etc/nix/nix.conf — it passes every option explicitly
  # (experimental-features, build-users-group "", sandbox false) and sets
  # HOME itself. includeNixDB: an in-image `nix build` needs the closure
  # REGISTERED in /nix/var/nix/db, not merely present in /nix/store.
  #
  # The tree is LITERAL: this file, ./worker, and a flake.lock derived from
  # the main flake.lock (std/refresh.sh; verified by tests/std-lint).
  description = "caos std/flake-builder — builds flake images into runnable images";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          script = pkgs.writeTextFile {
            name = "caos-worker-flake-builder-script";
            executable = true;
            destination = "/worker";
            text = builtins.readFile ./worker;
          };
        in
        pkgs.dockerTools.buildLayeredImage {
          # The caos-worker-<name> form: build-builtins.sh maps tarballs
          # back to std entries by this name.
          name = "caos-worker-flake-builder";
          tag = "latest";
          contents = [
            (pkgs.buildEnv {
              name = "caos-worker-flake-builder-root";
              paths = [
                script
                pkgs.nix
                # /etc/ssl/certs/ca-bundle.crt, for nix's https fetchers
                # (flake inputs from github, substitutes from
                # cache.nixos.org).
                pkgs.cacert
                pkgs.bashInteractive
                pkgs.coreutils
                # grep is NOT in coreutils; the homeless-shelter retry
                # (worker: nix_build_flake) greps the build error. The
                # stock nixos/nix base carried it; self-contained must too.
                pkgs.gnugrep
                pkgs.gzip
                # The deps memo (design/flake-deps-image.md) unpacks image
                # layers into the store; coreutils has no tar.
                pkgs.gnutar
                pkgs.skopeo
                pkgs.jq
              ];
            })
          ];
          config = {
            Entrypoint = [
              "/bin/caos"
              "runner"
            ];
            Env = [
              "PATH=/bin"
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              "NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              # Root: the nix store is root-owned (the same per-image
              # containment grant the test stack carries).
              "CAOS_WORKER_UID=0"
              "CAOS_WORKER_GID=0"
            ];
          };
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
