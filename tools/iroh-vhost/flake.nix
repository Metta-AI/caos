{
  description = "iroh-vhost: reach a dumbpipe listen-tcp target from a browser, ticket in the hostname";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        iroh-vhost = pkgs.rustPlatform.buildRustPackage {
          pname = "iroh-vhost";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          meta.mainProgram = "iroh-vhost";
        };
        default = iroh-vhost;
      });
      devShells = forAll (pkgs: {
        default = pkgs.mkShell { packages = [ pkgs.cargo pkgs.rustc pkgs.dumbpipe ]; };
      });
    };
}
