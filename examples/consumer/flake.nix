{
  description = "Example: using caos from another tree";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    # Point this at the caos repo. Here it's a relative path so the example
    # tracks this checkout; in a real consumer use a git URL, e.g.
    #   caos.url = "github:redheron/caos";
    # You consume caos's *already-built* outputs (caos-cli, the stack apps), so
    # there's nothing to recompile — a binary cache hit, not a rebuild.
    caos.url = "path:../..";
  };

  outputs =
    { self, nixpkgs, caos }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      caosPkgs = caos.packages.${system};
    in
    {
      # `nix develop` drops you into a shell with the caos commands on PATH —
      # no `nix run`/`nix build` needed, just run them directly:
      #   caosd      — bring the stack up (foreground; Ctrl-C stops it). It also
      #                (re)publishes the builtin worker library on each startup.
      #   caos-cli   — drive workers (run/get/curry/…)
      # caosd honors CAOS_DATA (default ./.caos-data) for the server's repo;
      # caos-cli must run inside a git working tree (this one) that has the
      # server as its `caos` remote:
      #   git remote add caos "$CAOS_SERVER_URL"
      #   caos-cli run docker://... -- --arg:@=path
      devShells.${system}.default = pkgs.mkShell {
        # One package, both commands (caos-cli + caosd) on PATH.
        packages = [ caosPkgs.caos-tools ];
        CAOS_SERVER_URL = "http://localhost:9090";
      };
    };
}
