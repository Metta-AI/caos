{
  # std/testenv (design/flake-images.md, design/cargo-workers.md phase 3):
  # std/bash's sibling for jobs that run a whole INNER caos stack — same
  # script-runner /worker (./worker, a byte-identical checked-in copy of
  # std/bash/worker, the source of truth: fetch `worker1`, run it with
  # bash), plus git (the inner server's smart-HTTP transport and the
  # inner client repo), a private redis (the nested stack's result cache;
  # starts empty, dies with the job), and the docker client (an inner
  # runnerd delegates sibling containers to the outer engine over the
  # granted socket, phase 4). The CAOS_WORKER_UID=0 grant makes jobs on this
  # image run as ROOT — the inner stack requires it (setuid installs into
  # the chroot slots); per-image containment policy, every other worker
  # keeps the uid-1000 fence.
  #
  # This directory IS the published tree (literal trees, part 2): flake.nix,
  # flake.lock (derived from the main flake.lock by std/refresh.sh), worker
  # — build-builtins.sh copies it whole, and tests/std-lint verifies the
  # checked-in redundancies (the lock re-derives, the worker matches
  # std/bash's).
  description = "caos std/testenv — script worker + git/redis/docker + root, for nested-stack jobs";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          workerRoot = pkgs.runCommand "testenv-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${./worker} $out/worker
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "testenv";
          tag = "latest";
          contents = [
            workerRoot
            pkgs.bash
            pkgs.coreutils
            pkgs.diffutils
            pkgs.gnugrep
            pkgs.gnused
            pkgs.gnutar
            pkgs.findutils
            # JSON plumbing for worker scripts — std/refresh.sh's lock
            # derivation (tests/std-lint runs its --check in this image)
            # among them.
            pkgs.jq
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
