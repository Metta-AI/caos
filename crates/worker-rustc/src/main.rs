//! caos-worker-rustc: build a caos worker from Rust source.
//!
//! Given a Rust source file as `--src` and the runner image as `--runner`
//! (typically curried in), it compiles the source for glibc (gnu), linking the
//! vendored `worker-common`, and emits at `/cas/out` a ready-to-run worker:
//! `curry(runner, bin=<the compiled binary>)`. So the worker is *not* its own
//! image — it's the shared, warm-pooled runner ([`crates/worker-runner`]) bound
//! to this binary, which is what avoids a per-worker image (no convert / registry
//! push / app provision). `caos run` the result like any other image.
//!
//! So building a worker is itself a worker — and because the run is memoized on
//! `(this image, src, runner)`, recompiling unchanged source is a cache hit.
//!
//! It needs cargo/rustc + a C linker on PATH and the `worker-common` source
//! vendored at [`VENDOR_WORKER_COMMON`]; its image (`caos-worker-rustc`) bakes
//! all of that in (from the stock `rust:1-bookworm` base). User source may use
//! `std` + `worker_common` only — there's no crates.io access in the sandbox.

use std::fs;
use std::process::{Command, ExitCode};

use worker_common::{arg, caos, caos_curry, run_worker, scratch, Arg};

/// The vendored `worker-common` crate baked into this image, for user source to
/// depend on.
const VENDOR_WORKER_COMMON: &str = "/vendor/worker-common";

/// We build for glibc (gnu), not musl: the stock `rust:1-bookworm` base this
/// worker runs on ships only the gnu target and gcc (no musl target / musl-gcc),
/// so gnu is what compiles out of the box. The produced binary is glibc-dynamic
/// and runs in the glibc (debian-slim) runner. Detected at compile time so the
/// rustc worker targets the architecture it was built for (e.g. Apple Silicon).
const TARGET: &str = if cfg!(target_arch = "aarch64") {
    "aarch64-unknown-linux-gnu"
} else {
    "x86_64-unknown-linux-gnu"
};

fn main() -> ExitCode {
    run_worker("rustc", run)
}

fn run() -> Result<(), String> {
    // `--src` is the worker's Rust source (a single .rs file); `--runner` is the
    // runner image to bind the compiled binary into (its hash is enough — we never
    // read its content here, so no `get`).
    caos(["get", &arg("src")])?;

    let binary = compile(&arg("src"))?;
    // Stage the compiled binary as a CAS blob, then curry it into the runner. The
    // result is a runnable worker: `curry(runner, bin=<binary>)`.
    caos(["put", &binary, "/cas/bin"])?;
    let curried = caos_curry(&arg("runner"), &[("bin", Arg::Path("/cas/bin"))])?;
    // Materialize the curried worker at /cas/out — its hash is this run's result.
    caos(["get-hash", &curried, "/cas/out"])
}

/// Lay out a cargo project around the user's source (as `src/main.rs`), depending
/// on the vendored `worker-common`, and build it for glibc (gnu). Returns the
/// path to the compiled `worker` binary.
fn compile(src: &str) -> Result<String, String> {
    let proj = scratch("proj")?;
    fs::create_dir_all(proj.join("src")).map_err(|e| format!("creating src dir: {e}"))?;
    fs::copy(src, proj.join("src/main.rs")).map_err(|e| format!("copying source: {e}"))?;
    fs::write(proj.join("Cargo.toml"), cargo_toml())
        .map_err(|e| format!("writing manifest: {e}"))?;

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
