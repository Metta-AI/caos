{
  # std/runner (design/flake-images.md, design/runner-protocol.md): the ONE
  # warm, pooled base every binary worker runs on — std/<name> =
  # curry(runner, bin=<static musl binary>), so a worker rebuild ships one
  # small blob, never an image. The flake-builder's delta supplies the setuid
  # caos, the /worker trampoline (which stages and execs the curried bin),
  # /tmp and the user db; this flake carries only the USERLAND those binaries
  # — and the commands bash-tool runs for the agent — see at runtime. That
  # userland is the agent-visible environment: nixpkgs bash + the standard
  # file tools (formerly debian:stable-slim's, when the runner was a delta on
  # a stock base).
  #
  # The published tree is this file + a flake.lock derived from the main
  # flake.lock at publish (stage-tree.sh), so the nixpkgs pin cannot drift
  # from the root flake's.
  description = "caos std/runner — the pooled bin-worker base: shell + standard file tools";

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
          name = "runner";
          tag = "latest";
          # bash provides /bin/sh too. No Entrypoint: the flake-builder forces
          # the runner entrypoint, and appends the delta's :/bin to PATH.
          contents = [
            pkgs.bash
            pkgs.coreutils
            pkgs.diffutils
            pkgs.gnugrep
            pkgs.gnused
            pkgs.findutils
            pkgs.gnutar
            pkgs.gzip
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
