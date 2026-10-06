{
  # dev/resident-test: a script worker image that DECLARES ITSELF RESIDENT
  # (design/daemons.md), so a worker on it may call `caos next` instead of
  # exiting. It exists for tests/resident, and for nothing else: residency is
  # per image, declared by the image's author, and a caller cannot ask for it —
  # so the only way to test it is an image that says yes.
  #
  # Otherwise it is std/bash: ./worker is the same script interpreter (fetch
  # `worker1`, run it with bash). A resident `worker1` loops, so `bash` does not
  # return until the daemon leaves.
  #
  # The knobs are short on purpose. A test cannot wait out a production poll
  # or a production grace period:
  #   CAOS_RESIDENT_POLL_MS   how long one poll hangs, and so how quickly the
  #                           runner notices a worker that died between jobs
  #   CAOS_RESIDENT_GRACE_SECS SIGTERM to SIGKILL
  description = "caos dev/resident-test — a script worker image that declares itself resident, for tests/resident";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          workerRoot = pkgs.runCommand "resident-test-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${./worker} $out/worker
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "caos-resident-test";
          tag = "latest";
          # No Entrypoint: runnerd forces `/bin/caos runner`, which execs /worker.
          contents = [
            workerRoot
            pkgs.bash
            pkgs.coreutils
            pkgs.gnugrep
          ];
          config = {
            Env = [
              "PATH=/bin"
              "CAOS_RESIDENT=1"
              "CAOS_RESIDENT_POLL_MS=2000"
              "CAOS_RESIDENT_GRACE_SECS=2"
              "CAOS_RESIDENT_MAX_SECS=600"
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
