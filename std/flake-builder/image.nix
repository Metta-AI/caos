# THE flake-builder's definition (design/flake-images.md): what rides on top
# of the stock nixos/nix base (pinned by digest in ./base.ref) — the /worker
# stage script and the tools the bare base lacks. One definition, imported
# by the root flake, which wraps it with the caos additions and streams the
# compose at publish (build-builtins.sh). It lives here, in its std/
# directory, like every other entry's definition; only WHO BUILDS IT is
# special — the host, because resolution can't build the thing that builds
# flakes. (A self-contained flake.nix — nix from nixpkgs instead of the
# stock base — is the recorded follow-up; see the design doc's Part 2.)
#
# We bake only what the bare nix base lacks: the /worker script, skopeo
# (push + inspect — a general flake makes no `#skopeo` promise), jq (to
# massage the returned config), and bash/coreutils/gzip. nix comes from the
# base's profile (PATH).
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
      # nix from the nixos/nix base's profile; caos, skopeo, bash,
      # coreutils, gzip from our baked /bin. The git-docker convert builds
      # this image's config from our own config.json (it does not merge the
      # base's env), so the nix profile paths must be explicit.
      "PATH=/root/.nix-profile/bin:/nix/var/nix/profiles/default/bin:/bin"
      # Root: the base's nix store is root-owned (the same per-image
      # containment grant the test nixbuilder and testenv carry).
      "CAOS_WORKER_UID=0"
      "CAOS_WORKER_GID=0"
    ];
  };
}
