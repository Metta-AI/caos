{
  # std/testenv-base (design/flake-images.md, design/cargo-workers.md phase 3):
  # the bash script environment plus what a job needs to run a whole INNER
  # caos stack — git (the inner server's smart-HTTP transport and the inner
  # client repo), a private redis (the nested stack's result cache; starts
  # empty, dies with the job), and the docker client (an inner runnerd
  # delegates sibling containers to the outer engine over the granted socket,
  # phase 4). The CAOS_WORKER_UID=0 grant makes jobs on this image run as
  # ROOT — the inner stack requires it (setuid installs into the chroot
  # slots); per-image containment policy, every other worker keeps the
  # uid-1000 fence.
  #
  # A pure BASE like bash-base: no /worker, no caos — the flake-builder's
  # delta supplies those. std/testenv is curry(<this tree>,
  # bin=images/bash-worker.sh). The published tree is this file + a lock
  # derived from the main flake.lock at publish (stage-tree.sh).
  description = "caos std/testenv-base — script env + git/redis/docker for nested-stack jobs";

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
          name = "testenv-base";
          tag = "latest";
          contents = [
            pkgs.bash
            pkgs.coreutils
            pkgs.diffutils
            pkgs.gnugrep
            pkgs.gnused
            pkgs.gnutar
            pkgs.findutils
            pkgs.gitMinimal
            pkgs.redis
            # The slimmed moby client the runnerd image ships — runnerd only
            # ever shells out to `docker run`, never builds or composes.
            (pkgs.docker-client.override {
              buildxSupport = false;
              composeSupport = false;
            })
          ];
          config = {
            Env = [
              "PATH=/bin"
              # The root grant (see header): caos runner reads these and
              # skips the uid-1000 drop for this image's jobs.
              "CAOS_WORKER_UID=0"
              "CAOS_WORKER_GID=0"
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
