{
  # std/merge (design/flake-images.md; SPEC "Merging and conflict resolution"):
  # the git-bearing worker image. Unlike std/bash (a script interpreter whose
  # /worker runs the curried `worker1`), std/merge bakes the merge logic itself
  # as /worker (checked in right here) — it is a complete worker, not an
  # interpreter — because merge is one fixed operation, not an arbitrary script.
  # The contents are what that script leans on: a shell, coreutils, git (for
  # `git merge-tree --write-tree`, index-free and worktree-free), and tar (to
  # lift the merged tree out of the local odb). git is the whole reason this is
  # a distinct image: no other std worker carries it, and reproducing
  # merge-tree's conflict notation in gix (not even a dependency's feature) is
  # not worth it.
  #
  # The contract (std/flake-builder/worker): a flake defines everything about
  # the image except the caos additions (/bin/caos, the worker user,
  # /usr/bin/env). /worker included.
  #
  # This directory IS the published tree (literal trees, part 2): flake.nix,
  # worker (the lock is DEPped from the repo root and placed by the flake-builder) —
  # build-builtins.sh copies it whole, and tests/lint verifies the
  # checked-in redundancies.
  description = "caos std/merge — the git-bearing merge worker: /worker three-way-merges two commits";

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
            # gitMinimal carries the plumbing merge-tree needs, no more.
            pkgs.gitMinimal
            # `git archive <tree> | tar -x` lifts the merged tree onto disk so
            # `caos put` can re-ingest it.
            pkgs.gnutar
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
