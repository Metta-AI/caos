{
  description = "GitHub CLI runtime";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }: {
    packages = builtins.listToAttrs (map (system:
      let
        pkgs = import nixpkgs { inherit system; };
        arch = if system == "aarch64-linux" then "arm64" else "amd64";
        digest = if system == "aarch64-linux"
          then "2da13f8c46f2770237c744b341ab6be9f07508585a6762634c4a88aa355460bc"
          else "9ed103934fab0f90d3341fdfc4a342785396d39f5621fc7313a62602ce2b5462";
        stack = pkgs.fetchurl {
          url = "https://github.com/github/gh-stack/releases/download/v0.1.1/linux-${arch}";
          sha256 = digest;
        };
        root = pkgs.runCommand "github-runner" {} ''
          mkdir -p $out/opt
          install -m755 ${stack} $out/opt/gh-stack
          install -m755 ${./worker} $out/worker
        '';
      in {
        name = system;
        value.caosImage = pkgs.dockerTools.buildLayeredImage {
          name = "github-runner";
          tag = "latest";
          contents = [ root pkgs.bash pkgs.coreutils pkgs.gitMinimal pkgs.gh pkgs.cacert ];
          config.Env = [ "PATH=/bin" "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt" ];
        };
      }) [ "x86_64-linux" "aarch64-linux" ]);
  };
}
