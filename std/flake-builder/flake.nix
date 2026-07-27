{
  # std/flake-builder (design/flake-images.md): the image that builds flake
  # images — self-contained, defined like every other std entry. #caosImage
  # is the CLEAN image per the contract: a flake defines everything about
  # the image except the caos additions, /worker (the stage script)
  # included. Resolution never builds this flake — it IS the flake path, so
  # the host builds it: the ROOT flake calls this file's outputs and
  # consumes `lib.<system>.imageDef` (the image's ingredients), wraps them
  # WITH the additions, and build-builtins.sh streams the result FROM
  # scratch. One definition, two consumers that cannot drift.
  #
  # The image is SELF-CONTAINED: nix, its fetchers' CA certs, the /worker
  # stage script, and the tools it runs, all from this nixpkgs. The worker
  # script needs no /etc/nix/nix.conf — it passes every option explicitly
  # (experimental-features, build-users-group "", sandbox false) and sets
  # HOME itself. Every image built from imageDef must pass includeNixDB =
  # true: an in-image `nix build` needs the closure REGISTERED in
  # /nix/var/nix/db, not merely present in /nix/store.
  #
  # The tree is LITERAL: this file, ./worker, and a flake.lock derived from
  # the main flake.lock (std/refresh.sh; verified by tests/std-lint).
  description = "caos std/flake-builder — builds flake images into runnable images";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      imageDefFor =
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
        {
          root = pkgs.buildEnv {
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
              pkgs.gzip
              pkgs.skopeo
              pkgs.jq
            ];
          };
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
              # containment grant the test nixbuilder and testenv carry).
              "CAOS_WORKER_UID=0"
              "CAOS_WORKER_GID=0"
            ];
          };
        };
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forEach = f: builtins.listToAttrs (map (system: { name = system; value = f system; }) systems);
    in
    {
      packages = forEach (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          def = imageDefFor system;
        in
        {
          caosImage = pkgs.dockerTools.buildLayeredImage {
            name = "flake-builder";
            tag = "latest";
            contents = [ def.root ];
            config = def.config;
            includeNixDB = true;
          };
        }
      );
      # The image's ingredients, for the root flake (the actual builder of
      # the streamed image — it adds the caos additions to these).
      lib = forEach (system: {
        imageDef = imageDefFor system;
      });
    };
}
