{
  # std/git-runner: the opt-in interpreter for compiled workers that need to
  # speak Git directly. It deliberately parallels std/runner's userland without
  # changing that shared, seeded image: only a worker whose expression selects
  # this runtime pays for gitMinimal.
  #
  # This directory is an ordinary literal flake tree. The flake-builder builds
  # it lazily, and /worker below execs the Rust binary supplied as `worker1`.
  description = "caos std/git-runner — opt-in Git runtime for compiled workers";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          workerRoot = pkgs.runCommand "git-runner-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${./worker} $out/worker
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "git-runner";
          tag = "latest";
          # No Entrypoint: runnerd forces `/bin/caos runner`, which execs
          # /worker. gitMinimal includes the smart-HTTP remote helpers.
          contents = [
            workerRoot
            pkgs.bash
            pkgs.coreutils
            pkgs.gitMinimal
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
