# THE flake-builder's definition (design/flake-images.md): SELF-CONTAINED —
# nix, its fetchers' CA certs, the /worker stage script, and the tools it
# runs, all from this nixpkgs. One definition, two consumers that cannot
# drift: ./flake.nix (the clean #caosImage, per the contract — everything
# but the caos additions) and the root flake, which wraps it WITH the
# additions and streams it at publish (build-builtins.sh, FROM scratch).
# It lives here, in its std/ directory, like every other entry's
# definition; only WHO BUILDS IT is special — the host, because resolution
# can't build the thing that builds flakes.
#
# The worker script needs no /etc/nix/nix.conf: nixf passes every option
# explicitly (experimental-features, build-users-group "", sandbox false)
# and sets HOME itself. Both image call sites must pass includeNixDB =
# true: an in-image `nix build` needs the closure REGISTERED in
# /nix/var/nix/db, not merely present in /nix/store.
{ pkgs }:
let
  script = pkgs.writeTextFile {
    name = "caos-worker-flake-builder-script";
    executable = true;
    destination = "/worker";
    text = builtins.readFile ./worker;
  };
in
{
  root = pkgs.buildEnv {
    name = "caos-worker-flake-builder-root";
    paths = [
      script
      pkgs.nix
      # /etc/ssl/certs/ca-bundle.crt, for nix's https fetchers (flake
      # inputs from github, substitutes from cache.nixos.org).
      pkgs.cacert
      pkgs.bashInteractive
      pkgs.coreutils
      pkgs.gzip
      pkgs.skopeo
      pkgs.jq
    ];
  };
  config = {
    Entrypoint = [
      "/bin/caos"
      "runner"
    ];
    Env = [
      "PATH=/bin"
      "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
      "NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
      # Root: the nix store is root-owned (the same per-image containment
      # grant the test nixbuilder and testenv carry).
      "CAOS_WORKER_UID=0"
      "CAOS_WORKER_GID=0"
    ];
  };
}
