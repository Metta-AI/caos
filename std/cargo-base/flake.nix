{
  # std/cargo-base as a FLAKE (design/flake-images.md, finding B): the pinned
  # rust toolchain + the caos workspace's dependencies pre-compiled for
  # (musl, dev). The flake-builder turns this tree into the clean base image
  # `std/cargo` and `std/rustc` curry their worker binaries onto; /worker and
  # caos arrive via the builder's caos delta, never here — so this image is
  # keyed on (toolchain, manifests, lockfile) alone and a caos source edit
  # re-ships one small binary blob, not these layers.
  #
  # The tree this flake ships in is GENERATED at publish by stage-tree.sh
  # (build-builtins.sh): this file + a flake.lock DERIVED from the main
  # flake.lock + the workspace's rust-toolchain.toml, Cargo.toml/Cargo.lock and
  # member manifests — no source, so a source edit never re-keys the tree
  # (registry memo flake-<H>). Deriving the lock is what pins this flake's
  # rust-overlay/nixpkgs/crane to the main flake's revisions: the toolchain
  # below resolves the same rustc the caos build uses, which the caos-in-caos
  # suite (a cargo worker building caos itself) depends on.
  description = "caos std/cargo-base — pinned toolchain + baked workspace deps (musl, dev)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      crane,
    }:
    let
      # The flake-builder builds for its own (Linux) architecture; carry both.
      forSystem =
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };

          muslTarget =
            if pkgs.stdenv.hostPlatform.isAarch64 then
              "aarch64-unknown-linux-musl"
            else
              "x86_64-unknown-linux-musl";

          # The same toolchain expression as the main flake's
          # cargoWorkerToolchain: `minimal` (rustc+cargo+std — no
          # clippy/rustfmt/rust-src in the image) at the channel
          # rust-toolchain.toml names, resolved by the SAME rust-overlay
          # revision (the derived lock) — so the version always matches the
          # toolchain that builds caos. The musl std rides along so produced
          # binaries can be static.
          channel = (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain.channel;
          byChannel = if channel == "stable" then pkgs.rust-bin.stable.latest else pkgs.rust-bin.stable.${channel};
          toolchain = byChannel.minimal.override { targets = [ muslTarget ]; };
          craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

          # The vendored crates.io sources for Cargo.lock, plus crane's
          # source-replacement config pointing at them. A store path — the same
          # absolute path at bake time and in-worker, which the fingerprints
          # require.
          vendor = craneLib.vendorCargoDeps { src = ./.; };

          # A musl C cross-compiler: rustc links musl self-contained, but
          # C-carrying deps (ring) compile via cc-rs, which needs a real musl
          # cc. The env var must be identical at bake time and in-worker, or
          # the baked fingerprints go stale (rerun-if-env-changed).
          muslCrossCC =
            if pkgs.stdenv.hostPlatform.isAarch64 then
              pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc
            else
              pkgs.pkgsCross.musl64.stdenv.cc;
          muslCCEnvName = "CC_${builtins.replaceStrings [ "-" ] [ "_" ] muslTarget}";
          muslCCEnv = "${muslCCEnvName}=${muslCrossCC}/bin/${muslCrossCC.targetPrefix}cc";

          # The one workspace-deps bake, (musl, dev), same knobs as the main
          # flake's cargoWorkerDepsMusl (see there for the full rationale). The
          # src here is the manifests-only bake tree; crane builds against its
          # own dummy sources either way, so the result is the same bake.
          deps = craneLib.buildDepsOnly (
            {
              src = ./.;
              pname = "caos-cargo-musl";
              version = "0.1.0";
              strictDeps = true;
              cargoVendorDir = vendor;
              CARGO_PROFILE = "dev";
              # Smaller debuginfo; the image env repeats it — a profile-key
              # mismatch is a silent full rebuild.
              CARGO_PROFILE_DEV_DEBUG = "line-tables-only";
              CARGO_BUILD_TARGET = muslTarget;
              cargoExtraArgs = "--locked --workspace";
              # Record both absolute paths Cargo fingerprints. The worker must
              # use these exact locations: build-script executables and their
              # OUT_DIRs are keyed on the target directory too.
              postInstall = ''
                wsroot=$(pwd -P)
                echo -n "$wsroot" > $out/ws-root
                targetdir="''${CARGO_TARGET_DIR:-target}"
                case "$targetdir" in /*) ;; *) targetdir="$wsroot/$targetdir" ;; esac
                echo -n "$targetdir" > $out/target-dir
              '';
            }
            // {
              ${muslCCEnvName} = "${muslCrossCC}/bin/${muslCrossCC.targetPrefix}cc";
            }
          );

          # The image root carries only the two recorded paths; the baked
          # target/ is inflated as real, writable files in fakeRootCommands —
          # a store symlink would be read-only, and the worker must overwrite
          # the dummy workspace-crate artifacts in place.
          rootEnv = pkgs.runCommand "cargo-base-root" { } ''
            mkdir -p $out
            cp ${deps}/ws-root $out/ws-root
            cp ${deps}/target-dir $out/target-dir
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "cargo-base";
          tag = "latest";
          contents = [ rootEnv ];
          config = {
            # No Entrypoint, no /bin: the flake-builder forces the runner
            # entrypoint and appends :/bin (the stacked caos) to this PATH.
            Env = [
              # The pinned toolchain, a C linker (the same cc-wrapper the
              # bake's build scripts linked under), the musl cross cc, and git
              # (the workspace's git-spawning unit tests) — all riding the
              # image closure via these references.
              "PATH=${toolchain}/bin:${pkgs.stdenv.cc}/bin:${muslCrossCC}/bin:${pkgs.gitMinimal}/bin"
              # A writable home; the worker copies the vendor config here.
              "CARGO_HOME=/tmp/cargo"
              "CAOS_VENDOR_CONFIG=${vendor}/config.toml"
              # Must match the bake above.
              "CARGO_PROFILE_DEV_DEBUG=line-tables-only"
              "${muslCCEnv}"
            ];
          };
          fakeRootCommands = ''
            wsroot=$(cat ws-root)
            targetdir=$(cat target-dir)
            # Inflate the baked (musl, dev) target/ as REAL, writable files at
            # the exact path the bake recorded. Crane archives the CONTENTS of
            # CARGO_TARGET_DIR, so extract at $targetdir, not $wsroot.
            mkdir -p ".$targetdir"
            ${pkgs.gnutar}/bin/tar --use-compress-program=${pkgs.zstd}/bin/zstd \
              -xf ${deps}/target.tar.zst -C ".$targetdir"
            # The whole workspace root must be owned and writable by the
            # worker (uid 1000): it materializes sources here and cargo
            # rewrites target/.
            chown -R 1000:1000 ".$wsroot"
            chmod -R u+w ".$wsroot"
          '';
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
