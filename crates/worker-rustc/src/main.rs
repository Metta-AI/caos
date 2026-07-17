//! caos-worker-rustc: build a caos worker from Rust source — as pure
//! orchestration over the cargo worker (design/cargo-workers.md, "rustc
//! re-layered on cargo"). No toolchain lives here: given `--src` (a single
//! .rs file) it lays out a cargo project as CAS links — the source as
//! `src/main.rs`, the `--worker_common` crate tree linked in, a generated
//! manifest — and tail-calls the cargo worker (`--cargo`, typically the
//! std/cargo curry) to compile it musl-static in release. The `finish`
//! continuation takes the built binary and emits at `/cas/out` a ready-to-run
//! worker: `curry(runner, bin=<the binary>)` — the shared, warm-pooled runner
//! bound to this binary, so the worker needs no image of its own. Static musl
//! means the binary runs on any base (the glibc runner today, scratch
//! eventually).
//!
//! So building a worker is itself a worker — memoized end to end: this run on
//! `(src, runner, cargo, worker_common)`, the inner compile on the project
//! tree. rustc itself runs as `curry(runner, bin=worker-rustc)` in the shared
//! pool; the old rust:1-bookworm rustc image is retired. User source may use
//! `std` + `worker_common` only — no crates.io deps.
//!
//! A failing user compile errors this run (with the cargo diagnostics), same
//! contract as the old in-image build.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_curry, link, own_image, path, read_arg, read_arg_opt, run_then, run_worker,
    scratch, Arg,
};

/// Build user workers static (musl): the binary then runs on any base. The
/// arch follows this worker's own build (e.g. Apple Silicon hosts).
const TARGET: &str = if cfg!(target_arch = "aarch64") {
    "aarch64-unknown-linux-musl"
} else {
    "x86_64-unknown-linux-musl"
};

fn main() -> ExitCode {
    run_worker("rustc", run)
}

fn run() -> Result<(), String> {
    // `finish` is reached only via our own curry in the run-then continuation.
    match read_arg_opt("mode")?.as_deref() {
        None | Some("") => start(),
        Some("finish") => finish(),
        Some(other) => Err(format!("unknown mode {other:?}")),
    }
}

/// Lay out the project (pure linking — nothing is fetched) and tail into the
/// cargo worker; `finish` gets the compile's result.
fn start() -> Result<(), String> {
    for required in ["src", "runner", "worker_common"] {
        if !Path::new(&arg(required)).exists() {
            return Err(format!("--{required} is required"));
        }
    }
    // The cargo worker's image ref rides as a literal (a hash string, read as
    // content), unlike `runner`/`worker_common` which ride as tree references.
    let cargo = read_arg("cargo")?;

    let proj = scratch("proj")?;
    fs::create_dir(proj.join("src")).map_err(|e| format!("creating src dir: {e}"))?;
    link(arg("src"), proj.join("src/main.rs"))?;
    link(arg("worker_common"), proj.join("worker-common"))?;
    fs::write(proj.join("Cargo.toml"), CARGO_TOML).map_err(|e| format!("writing manifest: {e}"))?;
    caos(["put", path(&proj), "/cas/proj"])?;

    let build = caos_curry(
        &cargo,
        &[
            ("cmd", Arg::Lit("build")),
            ("profile", Arg::Lit("release")),
            ("target", Arg::Lit(TARGET)),
        ],
    )?;
    // Ourselves, in the `finish` position: rebuild our own curry (the runner
    // image with our bin re-bound) plus what finish needs. `cargo` and
    // `worker_common` deliberately don't ride — finish's cache key is just
    // (bin, runner, result).
    let bin = arg("bin");
    let runner = arg("runner");
    let mut kvs: Vec<(&str, Arg)> =
        vec![("mode", Arg::Lit("finish")), ("runner", Arg::Path(&runner))];
    if Path::new(&bin).exists() {
        kvs.insert(0, ("bin", Arg::Path(&bin)));
    }
    let me = caos_curry(&own_image(), &kvs)?;
    run_then("/cas/proj", &build, Some(&me))
}

/// The compile came back: a failing build errors the run (diagnostics in the
/// message); a good one becomes `curry(runner, bin=<binary>)` at `/cas/out`.
fn finish() -> Result<(), String> {
    let res = arg("result");
    caos(["get", &res])?; // one level: exit/stderr/bin placeholders appear
    let exit = read_blob(&format!("{res}/exit"))?;
    if exit.trim() != "0" {
        let stderr = read_blob(&format!("{res}/stderr")).unwrap_or_default();
        return Err(format!(
            "cargo build failed (exit {}):\n{}",
            exit.trim(),
            stderr.trim_end()
        ));
    }
    let bin = format!("{res}/bin/worker");
    caos(["get", &format!("{res}/bin")])?; // the binary's placeholder
    if !Path::new(&bin).exists() {
        return Err("cargo result carries no bin/worker".to_string());
    }
    let curried = caos_curry(&arg("runner"), &[("bin", Arg::Path(&bin))])?;
    caos(["get-hash", &curried, "/cas/out"])
}

/// The generated manifest: the user's source as the `worker` binary, with the
/// linked-in `worker-common` as its one (path) dependency.
const CARGO_TOML: &str = "[package]\n\
     name = \"worker\"\n\
     version = \"0.0.0\"\n\
     edition = \"2021\"\n\
     \n\
     [[bin]]\n\
     name = \"worker\"\n\
     path = \"src/main.rs\"\n\
     \n\
     [dependencies]\n\
     worker-common = { path = \"worker-common\" }\n\
     \n\
     [profile.release]\n\
     strip = true\n";

/// Fetch and read a blob at a CAS path.
fn read_blob(cas_path: &str) -> Result<String, String> {
    caos(["get", cas_path])?;
    fs::read_to_string(cas_path).map_err(|e| format!("reading {cas_path}: {e}"))
}
