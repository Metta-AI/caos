//! caos-worker-cargo: whole-workspace `cargo check` / `build` / `test` (see
//! design/cargo-workers.md, phase 1). Input is a cargo workspace source tree
//! (`--tree`, or run-then's `in`) plus `--cmd` (check | build | test,
//! typically curried). The image bakes the pinned toolchain, the vendored
//! crates.io sources for the caos workspace's Cargo.lock, and that lockfile's
//! deps *pre-compiled* at a fixed workspace root — cargo fingerprints are
//! absolute-path-keyed (and toolchain-keyed), so bake and use share one image
//! and one path. The worker materializes the source tree at that root (fresh
//! mtimes, so workspace crates always rebuild — only the baked deps are
//! reused) and runs cargo `--offline`.
//!
//! Optional args: `--target` (a rustc target triple — e.g. musl, so a
//! produced binary is static and runs on any base) and `--profile` (default
//! `dev`; the caos deps bake is dev, other profiles compile from scratch).
//! Both ride the cache key like any arg.
//!
//! **Any cargo outcome is a value, never a worker error** — a compile error or
//! failing test is something the model must see and react to (and it caches:
//! same tree, same diagnostics). The result is a tree, the bash tool's shape
//! minus the workspace round-trip (cargo writes only target/, never the
//! source, so there is no staged-back `tree`):
//!
//! ```text
//! exit    blob  cargo's exit code, decimal (128+signal if killed)
//! stdout  blob  captured stdout, the last 100KB
//! stderr  blob  captured stderr, the last 100KB (cargo's diagnostics)
//! bin     tree  (cmd=build, exit 0) the produced executables, by name —
//!               what a worker-producing caller (rustc) is after
//! ```
//!
//! Only infrastructure failures (fetch failed, no baked workspace root) error
//! the run.
//!
//! A workspace whose lockfile differs from the baked one still builds — with
//! `--offline` against the baked vendor dir — but any dependency not in the
//! baked Cargo.lock fails resolution with cargo's "try without --offline"
//! error, which is the honest phase-0 boundary: changing deps needs a new
//! image (nix-built today; a cargo-deps worker later).

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::process::{Command, ExitCode};

use worker_common::{
    arg, caos, entries, file_name, path, read_arg, read_arg_opt, run_worker, scratch,
};

/// Keep at most this many bytes (the tail) of each captured stream.
const STREAM_CAP: usize = 100_000;

/// Written by the flake's image build: the absolute workspace root the deps
/// bake ran in (fingerprints pin it), holding the pre-compiled `target/`.
const WS_ROOT_FILE: &str = "/ws-root";

fn main() -> ExitCode {
    run_worker("cargo", run)
}

fn run() -> Result<(), String> {
    let cmd = read_arg("cmd")?;
    let target = read_arg_opt("target")?;
    let profile = read_arg_opt("profile")?.unwrap_or_else(|| "dev".to_string());
    let mut argv: Vec<&str> = match cmd.as_str() {
        "check" => vec!["check", "--workspace", "--all-targets"],
        "build" => vec!["build", "--workspace"],
        "test" => vec!["test", "--workspace"],
        other => return Err(format!("unknown cmd {other:?} (want check|build|test)")),
    };
    argv.extend(["--profile", &profile]);
    if let Some(t) = &target {
        argv.extend(["--target", t]);
    }

    // The workspace tree: run-then's `in`, or a direct `--tree` arg.
    let tree = if Path::new(&arg("in")).exists() {
        arg("in")
    } else {
        arg("tree")
    };
    if !Path::new(&tree).exists() {
        return Err(format!("no workspace tree at {tree} (pass --tree or in)"));
    }
    caos(["get", "-r", &tree])?; // the whole source tree, in full

    // Materialize it at the baked workspace root, beside the pre-compiled
    // target/. Fresh mtimes (fs::copy stamps now) keep cargo honest: newer
    // than every baked fingerprint, so workspace crates always recompile
    // while the deps stay fresh (their vendored sources sit at store epoch).
    let ws = fs::read_to_string(WS_ROOT_FILE)
        .map_err(|e| format!("reading {WS_ROOT_FILE}: {e}"))?
        .trim()
        .to_string();
    materialize(Path::new(&tree), Path::new(&ws))?;

    // Point cargo at the baked vendor dir: the same source-replacement config
    // the bake used, appended into a writable CARGO_HOME (the image env sets
    // CARGO_HOME=/tmp/cargo; the vendor config's store path is stable, which
    // is what keeps the deps fingerprints valid).
    let cargo_home = scratch("cargo")?;
    let vendor_config = std::env::var("CAOS_VENDOR_CONFIG")
        .map_err(|_| "CAOS_VENDOR_CONFIG not set (not the cargo worker image?)".to_string())?;
    fs::copy(&vendor_config, cargo_home.join("config.toml"))
        .map_err(|e| format!("copying vendor config: {e}"))?;

    let out = Command::new("cargo")
        .args(&argv)
        .arg("--offline")
        .current_dir(&ws)
        .output()
        .map_err(|e| format!("running cargo: {e}"))?;
    let exit = exit_code(&out.status);

    let res = scratch("result")?;
    fs::write(res.join("exit"), format!("{exit}\n")).map_err(|e| format!("writing exit: {e}"))?;
    fs::write(res.join("stdout"), tail(&out.stdout)).map_err(|e| format!("writing stdout: {e}"))?;
    fs::write(res.join("stderr"), tail(&out.stderr)).map_err(|e| format!("writing stderr: {e}"))?;
    if cmd == "build" && exit == 0 {
        stage_binaries(&ws, target.as_deref(), &profile, &res)?;
    }
    caos(["put", path(&res), "/cas/out"])
}

/// Stage the build's executables into `res/bin/<name>` — what a
/// worker-producing caller (rustc) is after. Cargo places a workspace's final
/// binaries directly in the profile dir (`target[/<triple>]/<debug|release>`);
/// everything else there (deps/, build/, .fingerprint/, *.d) is intermediate.
fn stage_binaries(ws: &str, target: Option<&str>, profile: &str, res: &Path) -> Result<(), String> {
    let mut dir = Path::new(ws).join("target");
    if let Some(t) = target {
        dir = dir.join(t);
    }
    // Cargo's dir for the `dev` profile is `debug`; other profiles use their
    // own name.
    dir = dir.join(if profile == "dev" { "debug" } else { profile });

    let mut bins = Vec::new();
    for entry in entries(path(&dir))? {
        let meta = fs::metadata(&entry).map_err(|e| format!("{}: {e}", entry.display()))?;
        if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
            bins.push(entry);
        }
    }
    if bins.is_empty() {
        return Ok(());
    }
    let bin_dir = res.join("bin");
    fs::create_dir(&bin_dir).map_err(|e| format!("creating {}: {e}", bin_dir.display()))?;
    for b in bins {
        fs::copy(&b, bin_dir.join(file_name(&b)))
            .map_err(|e| format!("staging {}: {e}", b.display()))?;
    }
    Ok(())
}

/// Copy the fetched source tree into the workspace root as real, writable
/// files with fresh mtimes. The root itself already exists (it holds the
/// baked `target/`); a top-level `target` entry in the input is skipped —
/// it's build output (gitignored in any sane workspace), and letting it
/// shadow the baked artifacts would defeat the image.
fn materialize(src: &Path, ws: &Path) -> Result<(), String> {
    for entry in entries(path(src))? {
        if file_name(&entry) == "target" {
            continue;
        }
        copy_into(&entry, &ws.join(file_name(&entry)))?;
    }
    Ok(())
}

fn copy_into(src: &Path, dst: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(src).map_err(|e| format!("{}: {e}", src.display()))?;
    if meta.file_type().is_symlink() {
        let dest = fs::read_link(src).map_err(|e| format!("{}: {e}", src.display()))?;
        symlink(&dest, dst).map_err(|e| format!("linking {}: {e}", dst.display()))?;
    } else if meta.is_dir() {
        fs::create_dir_all(dst).map_err(|e| format!("creating {}: {e}", dst.display()))?;
        for entry in entries(path(src))? {
            copy_into(&entry, &dst.join(file_name(&entry)))?;
        }
    } else {
        fs::copy(src, dst).map_err(|e| format!("copying {}: {e}", src.display()))?;
        fs::set_permissions(dst, fs::Permissions::from_mode(0o644))
            .map_err(|e| format!("chmod {}: {e}", dst.display()))?;
    }
    Ok(())
}

/// Cargo's exit code — or 128+signal when it died to one.
fn exit_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(-1)
}

/// The tail of a captured stream, capped at [`STREAM_CAP`] bytes (with a
/// marker so a truncated stream is recognizable as such).
fn tail(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() <= STREAM_CAP {
        return bytes.to_vec();
    }
    let mut out = b"[... truncated ...]\n".to_vec();
    out.extend_from_slice(&bytes[bytes.len() - STREAM_CAP..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn tail_caps_and_marks() {
        let big = vec![b'x'; STREAM_CAP + 10];
        let t = tail(&big);
        assert!(t.starts_with(b"[... truncated ...]\n"));
        assert_eq!(t.len(), STREAM_CAP + "[... truncated ...]\n".len());
        assert_eq!(tail(b"small"), b"small");
    }

    #[test]
    fn copy_into_copies_files_and_dirs() {
        let src = PathBuf::from("/tmp/wc-test-src");
        let dst = PathBuf::from("/tmp/wc-test-dst");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
        fs::create_dir_all(src.join("d")).unwrap();
        fs::write(src.join("d/f"), b"hi").unwrap();
        copy_into(&src, &dst).unwrap();
        assert_eq!(fs::read(dst.join("d/f")).unwrap(), b"hi");
    }
}
