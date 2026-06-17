//! caos: client for the object server.
//!
//! Subcommands:
//!
//! * `get-hash <hash> <path>` — fetch the git object `<hash>` from the object
//!   server (its base URL comes from `$CAOS_OBJECT_SERVER_URL`) and materialize
//!   it under `<path>`, which must be a direct child of `/cas`:
//!     - a blob  → write its bytes to `<path>`;
//!     - a tree  → create the directory `<path>` and, for each entry, an empty
//!       file named after that entry.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Base URL of the object server, e.g. `http://caos-object-server:8080`.
const OBJECT_SERVER_ENV: &str = "CAOS_OBJECT_SERVER_URL";

/// Directory under which objects are materialized. Override (e.g. for local
/// runs outside the container) with `CAOS_CAS_DIR`.
const CAS_DIR_ENV: &str = "CAOS_CAS_DIR";
const DEFAULT_CAS_DIR: &str = "/cas";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}: {err}", prog_name(&args));
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        Some("get-hash") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(hash), Some(path), None) => get_hash(hash, path),
            _ => Err(usage(args)),
        },
        _ => Err(usage(args)),
    }
}

/// Program name from `argv[0]` (`caos` in the image, `client` from the build
/// tree), for diagnostics and usage.
fn prog_name(args: &[String]) -> &str {
    args.first()
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .unwrap_or("caos")
}

fn usage(args: &[String]) -> String {
    format!("usage: {} get-hash <hash> <path>", prog_name(args))
}

/// Fetch `<hash>` from the object server and materialize it at `<path>`.
fn get_hash(hash: &str, path: &str) -> Result<(), String> {
    let base = std::env::var(OBJECT_SERVER_ENV)
        .map_err(|_| format!("{OBJECT_SERVER_ENV} must be set to the object-server URL"))?;
    let target = validate_target(path)?;

    let url = format!("{}/object/{hash}", base.trim_end_matches('/'));
    let data = http_get(&url)?;

    // The object server returns an object's content with no type header, so we
    // recover the type by parsing: valid tree bytes ⇒ directory, otherwise blob.
    // An empty object is treated as a blob — an empty blob and an empty tree are
    // indistinguishable by content, and a 0-byte object is virtually always a
    // blob.
    if !data.is_empty() {
        if let Ok(tree) = gix::objs::TreeRef::from_bytes(&data, gix::hash::Kind::Sha1) {
            return write_tree(&target, &tree);
        }
    }
    write_file(&target, &data)
}

/// Resolve `<path>` and require it to be a direct child of the CAS directory
/// (`/cas/foo`, never `/cas/foo/bar` or a path outside `/cas`).
fn validate_target(path: &str) -> Result<PathBuf, String> {
    let cas = PathBuf::from(std::env::var(CAS_DIR_ENV).unwrap_or_else(|_| DEFAULT_CAS_DIR.into()));
    let target = PathBuf::from(path);

    if target.parent() != Some(cas.as_path()) || target.file_name().is_none() {
        return Err(format!(
            "path must be a direct child of {} (e.g. {}/foo), got: {path}",
            cas.display(),
            cas.display()
        ));
    }
    Ok(target)
}

/// Blob → write its bytes verbatim to `target`.
fn write_file(target: &Path, data: &[u8]) -> Result<(), String> {
    std::fs::write(target, data).map_err(|e| format!("writing file {}: {e}", target.display()))
}

/// Tree → create `target` as a directory and an empty file per entry.
fn write_tree(target: &Path, tree: &gix::objs::TreeRef) -> Result<(), String> {
    std::fs::create_dir(target)
        .map_err(|e| format!("creating directory {}: {e}", target.display()))?;

    for entry in &tree.entries {
        let child = target.join(OsStr::from_bytes(entry.filename));
        std::fs::File::create(&child).map_err(|e| format!("creating {}: {e}", child.display()))?;
    }
    Ok(())
}

/// HTTP GET returning the raw response body. Non-2xx responses are errors.
fn http_get(url: &str) -> Result<Vec<u8>, String> {
    let response = minreq::get(url)
        .send()
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "GET {url}: server returned {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    Ok(response.into_bytes())
}
