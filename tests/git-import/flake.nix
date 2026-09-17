{
  description = "HTTPS Git import regression fixture";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }: {
    packages = builtins.listToAttrs (map (system:
      let pkgs = import nixpkgs { inherit system; }; in {
        name = system;
        value.caosImage = pkgs.dockerTools.buildLayeredImage {
          name = "caos-test-git-import";
          tag = "latest";
          contents = [
            (pkgs.runCommand "git-import-worker" {} ''
              mkdir -p $out
              install -m 755 ${./worker} $out/worker
            '')
            pkgs.bash pkgs.coreutils pkgs.gitMinimal pkgs.python3 pkgs.openssl
          ];
          config.Env = [ "PATH=/bin" ];
        };
      }) [ "x86_64-linux" "aarch64-linux" ]);
  };
}
