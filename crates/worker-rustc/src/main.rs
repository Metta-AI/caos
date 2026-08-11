//! caos-worker-rustc: build a caos worker from Rust source — as pure
//! orchestration over the cargo worker (design/cargo-workers.md, "rustc
//! re-layered on cargo"). No toolchain lives here: given `--src` (a single
//! .rs file, OR a whole project directory with its own `Cargo.toml`) it lays
//! out a cargo project as CAS links — the source as `src/main.rs` (or the
//! directory's own tree), the `--worker-common` crate tree linked in at
//! `worker-common/`, a generated manifest for the single-file case — and
//! tail-calls the cargo worker (`--cargo`, typically the std/cargo curry) to
//! compile it musl-static at the image's default (dev) profile, so a tool's
//! dependencies reuse the cargo image's seeded, precompiled `target/` (the
//! bake) rather than recompiling. The `finish` continuation takes the built
//! binary and emits at `/cas/out` a ready-to-run worker:
//! `curry(runner, worker1=<the binary>)` — the shared, warm-pooled runner it
//! DEPENDS on (std/rustc/DEPS) and binds itself, so no caller passes one —
//! bound to this binary, so the worker needs no image of its own. Static musl
//! means the binary runs on any base (the glibc runner today, scratch
//! eventually).
//!
//! So building a worker is itself a worker — memoized end to end: this run on
//! `(src, cargo, runner, worker-common)` — the bound `cargo`, `runner` and
//! `worker-common` are its LINKER INPUTS, wired in at publish
//! (build-builtins.sh; see
//! design/flake-images.md "rustc: the worker factory") — the inner compile on
//! the project tree. rustc itself runs as
//! `curry(runner, worker1=worker-rustc)` in the shared
//! pool; the old rust:1-bookworm rustc image is retired. User source may use
//! `std` + `worker_common` only — no crates.io deps.
//!
//! A failing user compile errors this run (with the cargo diagnostics), same
//! contract as the old in-image build.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use worker_common::{
    arg, caos, caos_curry, entries, file_name, link, own_image, path, read_arg, read_arg_opt,
    run_then, run_worker, scratch, Arg,
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
///
/// `--src` is either a single `.rs` file (a bare worker: we generate a manifest
/// with `worker-common` as its one path dep) or a whole PROJECT DIRECTORY (its
/// own `Cargo.toml` + `src/` — so a tool can declare extra deps, e.g. `regex`,
/// that ride the cargo image's baked, seeded `target/`). Either way
/// `worker-common` is spliced in at `worker-common/` as a path dep, and the
/// compile runs at the cargo image's DEFAULT profile (dev) + musl target,
/// MATCHING the bake so it reuses the seeded dependency artifacts instead of
/// recompiling them (design/caos-expr.md, Phase 3; std/cargo/bake.nix).
fn start() -> Result<(), String> {
    for required in ["src", "worker-common"] {
        if !Path::new(&arg(required)).exists() {
            return Err(format!("--{required} is required"));
        }
    }
    // The cargo worker's image ref rides as a literal (a hash string, read as
    // content): rustc is a SEEDED core item, so bootstrap hand-builds its curry
    // binding `--cargo=<cargo image hash>` (design/caos-expr.md, Phase 3).
    let cargo = read_arg("cargo")?;
    // The runner arrives the SAME way, and is NOT a caller's argument: rustc
    // DEPENDS on the runner (std/rustc/DEPS) and curries onto it itself, so a
    // caller says only what it is building. Every tool used to repeat
    // `--runner:@=DEEP-DEPS/runner`, which made the pool base part of every
    // tool's interface for no reason — and, once `runner` became a seeded
    // sentinel entry, a `:@=` path could not have named its image anyway.
    let runner = read_arg("runner")?;

    let proj = scratch("proj")?;
    let src = arg("src");
    // Fetch one level so we can tell a file from a project directory.
    caos(["get", &src])?;
    if Path::new(&src).is_dir() {
        // A whole project: link each of its top-level entries into proj (by CAS
        // hash — `caos put` resolves the links), then use its OWN Cargo.toml.
        // SKIP caos metadata: a deep-deps-shaped std tool carries `.caos-expr`,
        // `DEPS`, and a `DEEP-DEPS/` subtree of its (deepened) dependencies — none
        // of which is part of its cargo project. `DEEP-DEPS/` especially must not
        // reach cargo: it holds other tools' trees with their own nested
        // `Cargo.toml`s, which would derail package/workspace discovery.
        for entry in entries(&src)? {
            let name = file_name(&entry);
            if name == "DEEP-DEPS" || name == ".caos-expr" || name == "DEPS" {
                continue;
            }
            link(&entry, proj.join(name))?;
        }
        if !proj.join("Cargo.toml").exists() {
            return Err("--src is a directory but has no Cargo.toml".to_string());
        }
    } else {
        // A single .rs file: the source is src/main.rs and we generate a manifest.
        fs::create_dir(proj.join("src")).map_err(|e| format!("creating src dir: {e}"))?;
        link(&src, proj.join("src/main.rs"))?;
        fs::write(proj.join("Cargo.toml"), CARGO_TOML)
            .map_err(|e| format!("writing manifest: {e}"))?;
    }
    // worker-common as a path dep at a fixed location — the project's manifest
    // names it `worker-common = { path = "worker-common" }`.
    link(arg("worker-common"), proj.join("worker-common"))?;
    // Extra code dependencies: numbered `--dep0`/`--dep1`/… args, each a crate
    // tree (a deep-deps mount, e.g. DEEP-DEPS/llm-client). Numbered, not a
    // repeated `--dep`, which would collide at /cas/args/dep. Each is spliced at
    // its OWN package name (read from its Cargo.toml) so the tool's manifest
    // names it naturally (`llm-client = { path = "llm-client" }`). This is how a
    // tool declares a shared local crate beyond worker-common (design/caos-expr.md).
    let mut i = 0;
    loop {
        let dep = arg(&format!("dep{i}"));
        if !Path::new(&dep).exists() {
            break;
        }
        let name = dep_crate_name(&dep)?;
        link(&dep, proj.join(&name))?;
        i += 1;
    }
    caos(["put", path(&proj), "/cas/proj"])?;

    // No --profile: the cargo image's default is dev, which is what the bake
    // (musl, dev) precompiled the dependency graph at — so the tool's deps are
    // reused from the seeded target/ rather than recompiled. musl still links
    // static regardless of profile, so the binary runs on any base.
    let build = caos_curry(&cargo, &[("cmd", Arg::Lit("build"))])?;
    // Ourselves, in the `finish` position: rebuild our own curry (the runner
    // image with our bin re-bound) plus what finish needs. `cargo` and
    // `worker-common` deliberately don't ride — finish's cache key is just
    // (bin, runner, result).
    let bin = arg("worker1");
    let mut kvs: Vec<(&str, Arg)> =
        vec![("mode", Arg::Lit("finish")), ("runner", Arg::Lit(&runner))];
    if Path::new(&bin).exists() {
        kvs.insert(0, ("worker1", Arg::Path(&bin)));
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
    // `read_arg`, not `arg`: the runner rides as a hash LITERAL (like `cargo`),
    // and `resolve_run_image` takes a bare hash — so this curries onto the
    // runner image itself, not onto the blob that names it.
    let curried = caos_curry(&read_arg("runner")?, &[("worker1", Arg::Path(&bin))])?;
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

/// The `[package] name` of a code-dep crate tree at CAS path `dep`, so it is
/// spliced into the project under its own name. The first `name = "…"` line in
/// its `Cargo.toml` is `[package].name` (the section is first by convention).
fn dep_crate_name(dep: &str) -> Result<String, String> {
    // Expand the dep dir one level so its Cargo.toml is a materializable
    // placeholder (args arrive one level at a time, like `--src`).
    caos(["get", dep])?;
    let text = read_blob(&format!("{dep}/Cargo.toml"))?;
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("name") {
            if let Some(val) = rest.trim_start().strip_prefix('=') {
                let name = val.trim().trim_matches('"');
                if !name.is_empty() {
                    return Ok(name.to_string());
                }
            }
        }
    }
    Err(format!("no [package] name in {dep}/Cargo.toml"))
}
