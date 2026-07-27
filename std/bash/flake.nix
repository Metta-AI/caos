{
  # std/bash (design/flake-images.md): the script worker — a complete
  # interpreter worker image. /worker is the script runner (./worker,
  # checked in right here — the source of truth; std/testenv carries a
  # byte-identical copy): it fetches the `worker1` arg — the script, the
  # next executable in the chain — and runs it with bash. The contents are
  # the shell and file tools a worker script leans on. Curry a script on
  # (`--worker1:@=…`) and run it like any image.
  #
  # The contract (std/flake-builder/worker): a flake defines everything about
  # the image except the caos additions. /worker included.
  #
  # This directory IS the published tree (literal trees, part 2): flake.nix,
  # flake.lock (derived from the main flake.lock by std/refresh.sh), worker
  # — build-builtins.sh copies it whole, and tests/std-lint verifies the
  # checked-in redundancies.
  description = "caos std/bash — the script worker: shell + file tools, /worker runs `worker1`";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          workerRoot = pkgs.runCommand "bash-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${./worker} $out/worker
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "bash";
          tag = "latest";
          # bash provides /bin/sh too. No Entrypoint: runnerd forces
          # `/bin/caos runner`, which execs /worker.
          contents = [
            workerRoot
            pkgs.bash
            pkgs.coreutils
            pkgs.diffutils
            pkgs.gnugrep
            pkgs.findutils
            # JSON plumbing for worker scripts — std/refresh.sh's lock
            # derivation (the std-lint suite check) among them.
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
