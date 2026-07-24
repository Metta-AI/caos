# The ONE cargo toolchain + workspace-deps bake (design/cargo-workers.md,
# design/flake-images.md finding B), shared by its two consumers:
#
#   - std/cargo-base/flake.nix — the published std flake; `src` is its own
#     generated tree (manifests + stubs, no source; stage-tree.sh).
#   - the root flake's cargoDepsImage — the test suite's D2 deps-only base;
#     `src` is the cleaned workspace.
#
# One definition means the two cannot diverge; each caller supplies its own
# locked inputs (the published flake's lock is derived from the root's at
# publish, so both resolve the same nixpkgs/rust-overlay/crane).
#
# The bake must be SELF-CONSISTENT: cargo fingerprints are keyed on the exact
# compiler build, and dep artifacts contain proc-macro dylibs / build-script
# binaries linked against the compiling toolchain's glibc — so the toolchain
# that baked the deps must be the toolchain that uses them at runtime. That is
# why `env` pins everything the worker sees to the exact store paths baked
# against.
{
  pkgs,
  crane,
  src,
  toolchainFile,
}:
let
  muslTarget =
    if pkgs.stdenv.hostPlatform.isAarch64 then
      "aarch64-unknown-linux-musl"
    else
      "x86_64-unknown-linux-musl";

  # `minimal` (rustc+cargo+host std — no clippy/rustfmt/rust-src in the image)
  # at the channel the workspace's rust-toolchain.toml names, resolved by the
  # caller's rust-overlay — so the version always matches the toolchain that
  # builds caos (which the caos-in-caos suite depends on). The musl std rides
  # along so produced binaries can be static.
  channel = (builtins.fromTOML (builtins.readFile toolchainFile)).toolchain.channel;
  byChannel = if channel == "stable" then pkgs.rust-bin.stable.latest else pkgs.rust-bin.stable.${channel};
  toolchain = byChannel.minimal.override { targets = [ muslTarget ]; };
  craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

  # The vendored crates.io sources for Cargo.lock, plus crane's
  # source-replacement config pointing at them. A store path — the same
  # absolute path at bake time and in-worker, which the fingerprints require.
  vendor = craneLib.vendorCargoDeps { inherit src; };

  # A musl C cross-compiler: rustc links musl self-contained, but C-carrying
  # deps (ring) compile via cc-rs, which needs a real musl cc. The env var must
  # be identical at bake time and in-worker, or the baked fingerprints go stale
  # (rerun-if-env-changed).
  muslCrossCC =
    if pkgs.stdenv.hostPlatform.isAarch64 then
      pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc
    else
      pkgs.pkgsCross.musl64.stdenv.cc;
  muslCCEnvName = "CC_${builtins.replaceStrings [ "-" ] [ "_" ] muslTarget}";
  muslCCEnv = "${muslCCEnvName}=${muslCrossCC}/bin/${muslCrossCC.targetPrefix}cc";

  # THE workspace-deps bake: every dependency pre-compiled for (musl, dev)
  # against crane's dummy workspace sources — keyed on manifests + lockfile
  # only, so source edits never re-bake. This one bake serves everything:
  # `build` and `test` both compile with `--target=<musl>` at the default
  # (dev) profile (caos-tools/build.sh), so a per-edit build recompiles only
  # the workspace crates, never the dep graph. musl still links static
  # regardless of profile, so the produced binaries run on any base. There is
  # deliberately no second (host, release) bake — a second bake gets a
  # different absolute build dir under the sandbox-off in-caos nix-builder,
  # and the two can't share the one target/ dir Cargo fingerprints against.
  deps = craneLib.buildDepsOnly (
    {
      inherit src;
      pname = "caos-cargo-musl";
      version = "0.1.0";
      strictDeps = true;
      cargoVendorDir = vendor;
      CARGO_PROFILE = "dev";
      # Smaller debuginfo (file:line in backtraces, no full DWARF). `env`
      # repeats it: a profile-key mismatch is a silent full rebuild.
      CARGO_PROFILE_DEV_DEBUG = "line-tables-only";
      CARGO_BUILD_TARGET = muslTarget;
      cargoExtraArgs = "--locked --workspace";
      # Record both absolute paths Cargo fingerprints. The worker must use
      # these exact locations: build-script executables and their OUT_DIRs are
      # keyed on the target directory too.
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
in
{
  inherit
    muslTarget
    toolchain
    vendor
    muslCrossCC
    muslCCEnvName
    muslCCEnv
    deps
    ;

  # The image root shared by both consumers: only the two recorded paths. The
  # baked target/ is deliberately NOT inflated into a store path — dockerTools
  # would symlink its files read-only into /nix/store, and the worker must
  # overwrite the dummy workspace-crate artifacts in place — hence `inflate`
  # below, run in the image's fakeRootCommands instead.
  rootEnv = pkgs.runCommand "cargo-bake-root" { } ''
    mkdir -p $out
    cp ${deps}/ws-root $out/ws-root
    cp ${deps}/target-dir $out/target-dir
  '';

  # The worker-side env: the pinned toolchain, a C linker (the same cc-wrapper
  # the bake's build scripts and proc macros linked under, so everything
  # resolves against one glibc), the musl cross cc, and git (the workspace's
  # git-spawning unit tests) — all riding the image closure via these
  # references. /bin is where each consumer's caos (and, for the D2 image, its
  # shell) lives.
  env = [
    "PATH=${toolchain}/bin:${pkgs.stdenv.cc}/bin:${muslCrossCC}/bin:${pkgs.gitMinimal}/bin:/bin"
    # A writable home; the worker copies the vendor config here.
    "CARGO_HOME=/tmp/cargo"
    "CAOS_VENDOR_CONFIG=${vendor}/config.toml"
    # Must match the bake above.
    "CARGO_PROFILE_DEV_DEBUG=line-tables-only"
    "${muslCCEnv}"
  ];

  # fakeRootCommands fragment: inflate the baked (musl, dev) target/ as REAL,
  # writable files at the exact path the bake recorded, owned by the worker
  # (uid 1000 — it materializes sources at the workspace root and cargo
  # rewrites target/). Crane archives the CONTENTS of CARGO_TARGET_DIR, so
  # extract at $targetdir, not $wsroot.
  inflate = ''
    wsroot=$(cat ws-root)
    targetdir=$(cat target-dir)
    mkdir -p ".$targetdir"
    ${pkgs.gnutar}/bin/tar --use-compress-program=${pkgs.zstd}/bin/zstd \
      -xf ${deps}/target.tar.zst -C ".$targetdir"
    chown -R 1000:1000 ".$wsroot"
    chmod -R u+w ".$wsroot"
  '';
}
