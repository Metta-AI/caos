//! caos-worker-rustc: build a caos worker image from Rust source.
//!
//! Given a Rust source file as `--src` and a worker-base git-docker image as
//! `--base` (typically curried in), it compiles the source — statically, for
//! musl, linking the vendored `worker-common` — and emits a new worker image at
//! `/cas/out`: the base's layers with one more layer carrying the compiled
//! `/worker` on top, plus a generated `config.json`. The result is an ordinary
//! worker image: `caos run` it like any other.
//!
//! So building a worker is itself a worker — and because the run is memoized on
//! `(this image, src, base)`, recompiling unchanged source is a cache hit.
//!
//! It needs cargo/rustc + a C linker on PATH and the `worker-common` source
//! vendored at [`VENDOR_WORKER_COMMON`]; its image (`caos-worker-rustc`) bakes
//! all of that in. User source may use `std` + `worker_common` only — there's no
//! crates.io access in the worker sandbox.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, ExitCode};

use worker_common::{arg, caos, entries, file_name, link, path, run_worker, scratch};

/// The vendored `worker-common` crate baked into this image, for user source to
/// depend on.
const VENDOR_WORKER_COMMON: &str = "/vendor/worker-common";

/// Static target, so the produced `/worker` needs no libc in its image.
const TARGET: &str = "x86_64-unknown-linux-musl";

fn main() -> ExitCode {
    run_worker("rustc", run)
}

fn run() -> Result<(), String> {
    // `--src` is the worker's Rust source (a single .rs file); `--base` is the
    // worker-base git-docker image to extend (one level is enough — we reference
    // its layers by hash, not their content).
    caos(["get", &arg("src")])?;
    caos(["get", &arg("base")])?;

    let binary = compile(&arg("src"))?;
    let layer = stage_layer(&binary)?;
    assemble_image(&arg("base"), &layer)
}

/// Lay out a cargo project around the user's source (as `src/main.rs`), depending
/// on the vendored `worker-common`, and build it static for musl. Returns the
/// path to the compiled `worker` binary.
fn compile(src: &str) -> Result<String, String> {
    let proj = scratch("proj")?;
    fs::create_dir_all(proj.join("src")).map_err(|e| format!("creating src dir: {e}"))?;
    fs::copy(src, proj.join("src/main.rs")).map_err(|e| format!("copying source: {e}"))?;
    fs::write(proj.join("Cargo.toml"), cargo_toml()).map_err(|e| format!("writing manifest: {e}"))?;

    let status = Command::new("cargo")
        .args(["build", "--release", "--offline", "--target", TARGET])
        .current_dir(&proj)
        .env("CARGO_HOME", "/tmp/cargo")
        .status()
        .map_err(|e| format!("running cargo: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build failed ({status})"));
    }
    Ok(format!("{}/target/{TARGET}/release/worker", proj.display()))
}

/// Stage the compiled binary as a single-entry image layer `{ worker: <bin> }`,
/// `caos put` it, and return its CAS path. The binary is executable, and a
/// root-owned executable is the git-docker default, so no perm sidecar is needed
/// — it converts to an executable `/worker`.
fn stage_layer(binary: &str) -> Result<String, String> {
    let dir = scratch("layer")?;
    let worker = dir.join("worker");
    fs::copy(binary, &worker).map_err(|e| format!("copying binary: {e}"))?;
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod worker: {e}"))?;
    caos(["put", path(&dir), "/cas/layer"])?;
    Ok("/cas/layer".to_string())
}

/// Build the output image tree: a generated `config.json`, the base image's
/// `layer<NN>` subtrees (reused by hash), and our new layer stacked on top.
fn assemble_image(base: &str, layer: &str) -> Result<(), String> {
    let out = scratch("out")?;
    fs::write(out.join("config.json"), image_config())
        .map_err(|e| format!("writing config.json: {e}"))?;

    // Carry the base's layers through unchanged and number ours just above them.
    let mut top = 0u64;
    for entry in entries(base)? {
        let name = file_name(&entry);
        if let Some(num) = name.strip_prefix("layer").and_then(|s| s.parse::<u64>().ok()) {
            link(&entry, out.join(&name))?;
            top = top.max(num + 1);
        }
    }
    link(layer, out.join(format!("layer{top:02}")))?;
    caos(["put", path(&out), "/cas/out"])
}

/// The generated Cargo manifest: the user's source as the `worker` binary, with
/// the vendored `worker-common` as its one dependency.
fn cargo_toml() -> String {
    format!(
        "[package]\n\
         name = \"worker\"\n\
         version = \"0.0.0\"\n\
         edition = \"2021\"\n\
         \n\
         [[bin]]\n\
         name = \"worker\"\n\
         path = \"src/main.rs\"\n\
         \n\
         [dependencies]\n\
         worker-common = {{ path = \"{VENDOR_WORKER_COMMON}\" }}\n\
         \n\
         [profile.release]\n\
         strip = true\n"
    )
}

/// A minimal OCI image config. The server fills `rootfs.diff_ids` when it
/// converts the image, so we leave them empty. `Env` carries `PATH` so the
/// worker can find the setuid `caos` at `/bin`.
fn image_config() -> String {
    r#"{"architecture":"amd64","os":"linux",
       "config":{"Entrypoint":["/bin/caos","entrypoint"],"Env":["PATH=/bin"]},
       "rootfs":{"type":"layers","diff_ids":[]}}"#
        .to_string()
}
