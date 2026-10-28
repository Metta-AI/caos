{
  # std/flake-update: `nix flake update`, as a tool. The image is nix and what
  # its fetchers need (CA certs for github: inputs, git for git+ ones), and
  # /worker is ./worker.go COMPILED HERE. A worker runs in one image, and this
  # one has to carry nix, so the Go worker cannot be a script on std/go; it is
  # built as a program of std/go's prelude module instead (DEEP-DEPS/prelude,
  # declared in DEPS), so it imports `caos/w` like any Go worker.
  #
  # The prelude's dependencies come from nix with the SAME `vendorHash` std/go
  # uses: adding a main package changes no dependency, so the hash is the
  # prelude's. When std/go's moves, this one must move with it.
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

          # The prelude module with this worker as its main package: the
          # layout std/go's /worker makes at run time, made once at build time.
          src = pkgs.runCommand "caos-worker-flake-update-src" { } ''
            mkdir -p $out
            cp -RL ${./DEEP-DEPS/prelude}/. $out/
            chmod -R u+w $out
            cp ${./worker.go} $out/worker.go
          '';
          worker = pkgs.buildGoModule {
            pname = "caos-worker-flake-update";
            version = "0";
            inherit src;
            vendorHash = "sha256-VCQFQiZLnz7QJhFWiXzolP+HkrIz7/qDYfoHNJ0x5No=";
            subPackages = [ "." ];
            # Static: nothing in this image is a libc to find at run time.
            CGO_ENABLED = 0;
          };
          # The module is `caos`, so its main package builds as bin/caos.
          workerRoot = pkgs.runCommand "caos-worker-flake-update-root" { } ''
            mkdir -p $out
            install -m 755 ${worker}/bin/caos $out/worker
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "flake-update";
          tag = "latest";
          # No Entrypoint: runnerd forces `/bin/caos runner`, which execs /worker.
          contents = [
            workerRoot
            pkgs.nix
            # /etc/ssl/certs/ca-bundle.crt, for nix's https fetchers.
            pkgs.cacert
            pkgs.coreutils
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
