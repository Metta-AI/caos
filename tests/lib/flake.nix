{
  # tests/lib: the image a CLIENT test runs in, with the client-repo setup baked
  # in as `/worker`.
  #
  # A test that drives `caos-cli` needs a git worktree, because `:@=` ingests
  # git-tracked paths and nothing else. Most tests do not — they are workers,
  # they name things by /cas path and hash, and they should not pay for a repo
  # they never read. So this is a DEPENDENCY a test declares (`../lib lib`) and
  # names as its base, which makes "which tests are client tests" a fact you can
  # grep for rather than a guess.
  #
  # THE IMAGE AND THE SCRIPT LIVE TOGETHER, deliberately. What a client test can
  # reach for and what the setup does with it are one decision — a test that
  # starts using `jq` needs this flake to carry jq, and there is no version of
  # that where the two are usefully in different directories.
  #
  # WHAT IT DELIBERATELY DOES NOT CARRY: any grant. `dev/test-stack` declares
  # persistent volumes and CAP_SYS_ADMIN because it hosts a stack; a test does
  # not, and runnerd honours grants per image — so running tests in that image
  # would hand every one of them write access to the dev stack's own nix store
  # and object database. A test that could corrupt the stack it is testing is a
  # bad afternoon.
  #
  # It also carries no caos client. `caos-cli` reaches a test as a curried ARG,
  # because which client a test drives is a property of the RUN — the tree under
  # test just compiled it — rather than of this image. That is what makes a test
  # re-key when the binary under test changes, which is the entire point of
  # having a suite.
  #
  # THE CONTENTS ARE WHAT A TEST SCRIPT SHELLS OUT TO, and `git` above all: a
  # client test drives caos through a repo it stages itself. An absent binary
  # here is exit 127 at runtime, not a build error, so the list errs generous.
  #
  # This directory IS the published tree (literal trees): flake.nix, flake.lock
  # (derived from the main flake.lock by std/refresh.sh), worker.
  description = "caos tests/lib — the image a CLIENT test runs in: a git worktree, the tested client, and the tools a test script shells out to";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          workerRoot = pkgs.runCommand "tests-lib-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${./worker} $out/worker
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "caos-tests-lib";
          tag = "latest";
          # No Entrypoint: runnerd forces `/bin/caos runner`, which execs
          # /worker — which is this setup.
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
