{
  # dev/run-test: the image ONE tests/<name> runs in, with the harness that runs
  # it baked in as `/worker`.
  #
  # THE IMAGE AND THE SCRIPT LIVE TOGETHER, deliberately. What a test can reach
  # for and what the harness does with it are one decision — a `cli.sh` that
  # starts using `jq` needs this flake to carry jq, and there is no version of
  # that where the two are usefully in different directories. So `run-test.sh`
  # is `/worker` here rather than a `worker1` curried on from elsewhere, and
  # evaluating this entry yields something `dev/run-tests` can map over directly.
  #
  # WHAT IT DELIBERATELY DOES NOT CARRY: any grant. `dev/test-stack` declares
  # persistent volumes, the engine socket and CAP_SYS_ADMIN because it hosts a
  # stack; a test does not, and runnerd honours grants per image, so running
  # tests in that image would hand every one of them write access to the dev
  # stack's own nix store and object database. Nothing here needs that, and a
  # test that could corrupt the stack it is testing is a bad afternoon.
  #
  # It also carries no caos client. `caos-cli` reaches a test as a curried ARG,
  # because which client a test drives is a property of the RUN — the tree under
  # test just compiled it — rather than of this image. That is what makes a test
  # re-key when the binary under test changes, which is the entire point of
  # having a suite.
  #
  # THE CONTENTS ARE WHAT A cli.sh SHELLS OUT TO, and `git` above all: a test
  # drives caos through a client repo it stages itself, which is ~314 git call
  # sites across the suite. An absent binary here is exit 127 at runtime, not a
  # build error, so the list errs generous.
  #
  # This directory IS the published tree (literal trees): flake.nix, flake.lock
  # (derived from the main flake.lock by std/refresh.sh), run-test.sh.
  description = "caos dev/run-test — the image one test runs in, with the harness as /worker";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          workerRoot = pkgs.runCommand "run-test-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${./run-test.sh} $out/worker
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "caos-run-test";
          tag = "latest";
          # No Entrypoint: runnerd forces `/bin/caos runner`, which execs
          # /worker — which is the harness.
          contents = [
            workerRoot
            pkgs.bashInteractive
            pkgs.coreutils
            # The client repo every test stages, and the pushes it makes.
            pkgs.gitMinimal
            pkgs.gnugrep
            pkgs.gnused
            pkgs.gawk
            pkgs.findutils
            # cmp/diff: several tests compare cached results byte for byte.
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
