//! caos: client for the object server.
//!
//! Subcommands:
//!
//! * `get-hash <hash> <path>` — fetch the git object `<hash>` from the object
//!   server (base URL from `$CAOS_OBJECT_SERVER_URL`) and materialize it at
//!   `<path>`, a direct child of `/cas`: a blob becomes a file holding its
//!   bytes; a tree becomes a directory holding one empty placeholder per entry
//!   (a directory for subtrees, a file otherwise).
//! * `get <path>` — expand an existing placeholder anywhere under `/cas`: read
//!   its recorded hash, fetch that object, and replace the empty file with the
//!   blob's content, or the empty directory with the tree's entries.
//!
//! Every materialized path is tagged with the git hash it came from in the
//! `user.caos.hash` extended attribute — the top-level path with `<hash>`, and
//! each child of a tree with that entry's own oid. This is both the on-disk,
//! per-path, thread-safe mapping from CAS paths back to hashes, and what lets
//! `get` expand a placeholder later.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gix::objs::WriteTo;

/// Base URL of the object server, e.g. `http://caos-object-server:8080`.
const OBJECT_SERVER_ENV: &str = "CAOS_OBJECT_SERVER_URL";

/// Directory under which objects are materialized. Override (e.g. for local
/// runs outside the container) with `CAOS_CAS_DIR`.
const CAS_DIR_ENV: &str = "CAOS_CAS_DIR";
const DEFAULT_CAS_DIR: &str = "/cas";

/// xattr recording the git hash a materialized path came from.
const HASH_XATTR: &str = "user.caos.hash";
/// xattr used only by the startup support probe.
const PROBE_XATTR: &str = "user.caos.probe";

/// Disambiguates temp names created within a single process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        Some("get") => match (args.get(2), args.get(3)) {
            (Some(path), None) => get(path),
            _ => Err(usage(args)),
        },
        Some("put") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(src), Some(dst), None) => put(src, dst),
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
    let prog = prog_name(args);
    format!(
        "usage:\n  {prog} get-hash <hash> <path>\n  {prog} get <path>\n  \
         {prog} put <src-path> <cas-path>"
    )
}

/// `get-hash <hash> <path>` — fetch `<hash>` and materialize it at `<path>`,
/// which must be a direct child of the CAS directory.
fn get_hash(hash: &str, path: &str) -> Result<(), String> {
    let base = object_server_url()?;
    let cas = cas_dir();
    let target = validate_target(&cas, path)?;
    probe_xattr(&cas)?;
    fetch_and_materialize(&base, &target, hash)
}

/// `get <path>` — re-materialize the object recorded at `<path>` (a path inside
/// the CAS directory, possibly deep). Reads `<path>`'s recorded hash, fetches
/// that object, and replaces the placeholder: an empty file with the blob's
/// content, or an empty directory with the tree's entries.
fn get(path: &str) -> Result<(), String> {
    let base = object_server_url()?;
    let cas = cas_dir();
    let target = validate_descendant(&cas, path)?;
    probe_xattr(&cas)?;
    let hash = read_hash(&target)?;
    fetch_and_materialize(&base, &target, &hash)
}

/// Fetch object `hash` and write it to `target` (blob → file, tree → directory).
fn fetch_and_materialize(base: &str, target: &Path, hash: &str) -> Result<(), String> {
    let url = format!("{}/object/{hash}", base.trim_end_matches('/'));
    let serialized = http_get(&url)?;

    // The server returns the serialized object (`<type> <size>\0<content>`), so
    // the type is authoritative — no guessing.
    let (kind, content) = parse_object(&serialized)?;
    if kind == "tree" {
        let tree = gix::objs::TreeRef::from_bytes(content, gix::hash::Kind::Sha1)
            .map_err(|e| format!("malformed tree {hash}: {e}"))?;
        write_tree(target, hash, &tree)
    } else {
        write_file(target, hash, content)
    }
}

/// Split a serialized git object (`<type> <size>\0<content>`) into its type and
/// content, validating the declared size.
fn parse_object(bytes: &[u8]) -> Result<(&str, &[u8]), String> {
    let nul = bytes
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| "object response missing NUL after header".to_string())?;
    let header =
        std::str::from_utf8(&bytes[..nul]).map_err(|e| format!("bad object header: {e}"))?;
    let content = &bytes[nul + 1..];

    let (kind, size) = header
        .split_once(' ')
        .ok_or_else(|| "bad object header: expected '<type> <size>'".to_string())?;
    let size: usize = size.parse().map_err(|e| format!("bad object size: {e}"))?;
    if size != content.len() {
        return Err(format!(
            "object size {size} != content length {}",
            content.len()
        ));
    }
    Ok((kind, content))
}

/// Base URL of the object server from the environment.
fn object_server_url() -> Result<String, String> {
    std::env::var(OBJECT_SERVER_ENV)
        .map_err(|_| format!("{OBJECT_SERVER_ENV} must be set to the object-server URL"))
}

/// `put <src-path> <cas-path>` — recursively store `<src-path>` (a path outside
/// the CAS) into the object server and record the result at `<cas-path>`, a
/// direct child of the CAS directory.
///
/// Files are stored as blobs and directories as trees. A symlink that resolves
/// to something already in the CAS is *not* re-read — its recorded hash is
/// reused, so shared content is stored once.
///
/// Note: the object server only writes blobs, so tree objects are stored as
/// their canonical git encoding under a blob hash. These aren't real git tree
/// hashes, but they round-trip through `get` (which recovers the type by
/// parsing the bytes).
fn put(src: &str, dst: &str) -> Result<(), String> {
    let base = object_server_url()?;
    let cas = cas_dir();
    let target = validate_target(&cas, dst)?;
    probe_xattr(&cas)?;
    let cas_real = cas
        .canonicalize()
        .map_err(|e| format!("CAS directory {}: {e}", cas.display()))?;

    let (_, oid) = store(&base, &cas_real, Path::new(src))?;
    fetch_and_materialize(&base, &target, &oid.to_string())
}

/// Recursively store `path` in the object server, returning the git tree entry
/// (mode + oid) that refers to it.
fn store(
    base: &str,
    cas_real: &Path,
    path: &Path,
) -> Result<(gix::objs::tree::EntryMode, gix::ObjectId), String> {
    use gix::objs::tree::EntryKind;

    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let ft = meta.file_type();

    if ft.is_symlink() {
        // A symlink that resolves into the CAS: reuse the hash recorded there
        // instead of re-reading the target.
        if let Ok(canon) = path.canonicalize() {
            if canon != cas_real && canon.starts_with(cas_real) {
                let kind = if canon.is_dir() {
                    EntryKind::Tree
                } else {
                    EntryKind::Blob
                };
                return Ok((kind.into(), parse_oid(&read_hash(&canon)?)?));
            }
        }
        // Otherwise store it as a git symlink: a blob holding the link target.
        let link = std::fs::read_link(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let oid = post_object(base, "blob", link.as_os_str().as_bytes())?;
        return Ok((EntryKind::Link.into(), oid));
    }

    if ft.is_dir() {
        let mut entries = Vec::new();
        for dirent in std::fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))? {
            let dirent = dirent.map_err(|e| format!("{}: {e}", path.display()))?;
            let (mode, oid) = store(base, cas_real, &dirent.path())?;
            entries.push(gix::objs::tree::Entry {
                mode,
                filename: dirent.file_name().into_vec().into(),
                oid,
            });
        }
        // Git requires tree entries in a specific order; Entry's Ord implements it.
        entries.sort();
        let mut buf = Vec::new();
        gix::objs::Tree { entries }
            .write_to(&mut buf)
            .map_err(|e| format!("encoding tree for {}: {e}", path.display()))?;
        let oid = post_object(base, "tree", &buf)?;
        return Ok((EntryKind::Tree.into(), oid));
    }

    if ft.is_file() {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let oid = post_object(base, "blob", &data)?;
        let kind = if meta.permissions().mode() & 0o111 != 0 {
            EntryKind::BlobExecutable
        } else {
            EntryKind::Blob
        };
        return Ok((kind.into(), oid));
    }

    Err(format!("unsupported file type: {}", path.display()))
}

/// POST a serialized git object (`<type> <size>\0<content>`) to the object
/// server and return its hash.
fn post_object(base: &str, kind: &str, content: &[u8]) -> Result<gix::ObjectId, String> {
    let mut body = format!("{kind} {}\0", content.len()).into_bytes();
    body.extend_from_slice(content);

    let url = format!("{}/object/", base.trim_end_matches('/'));
    let response = minreq::post(&url)
        .with_body(body)
        .send()
        .map_err(|e| format!("POST {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "POST {url}: server returned {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    let body = response
        .as_str()
        .map_err(|e| format!("POST {url}: invalid response: {e}"))?;
    parse_oid(body)
}

/// Parse a hex git hash (tolerating surrounding whitespace).
fn parse_oid(hex: &str) -> Result<gix::ObjectId, String> {
    gix::ObjectId::from_hex(hex.trim().as_bytes()).map_err(|e| format!("invalid hash {hex:?}: {e}"))
}

/// CAS root directory (`/cas`, or `$CAOS_CAS_DIR`).
fn cas_dir() -> PathBuf {
    PathBuf::from(std::env::var(CAS_DIR_ENV).unwrap_or_else(|_| DEFAULT_CAS_DIR.into()))
}

/// Resolve `<path>` and require it to be a direct child of the CAS directory
/// (`/cas/foo`, never `/cas/foo/bar` or a path outside `/cas`).
fn validate_target(cas: &Path, path: &str) -> Result<PathBuf, String> {
    let target = PathBuf::from(path);

    if target.parent() != Some(cas) || target.file_name().is_none() {
        return Err(format!(
            "path must be a direct child of {} (e.g. {}/foo), got: {path}",
            cas.display(),
            cas.display()
        ));
    }
    Ok(target)
}

/// Require an existing `<path>` strictly inside the CAS directory (any depth).
/// Canonicalizes, so symlinks and `..` can't escape the CAS root.
fn validate_descendant(cas: &Path, path: &str) -> Result<PathBuf, String> {
    let cas = cas
        .canonicalize()
        .map_err(|e| format!("CAS directory {}: {e}", cas.display()))?;
    let target = Path::new(path)
        .canonicalize()
        .map_err(|e| format!("{path}: {e}"))?;

    if target == cas || !target.starts_with(&cas) {
        return Err(format!(
            "path must be inside {}, got: {path}",
            cas.display()
        ));
    }
    Ok(target)
}

/// Read the git hash recorded in `path`'s `user.caos.hash` xattr.
fn read_hash(path: &Path) -> Result<String, String> {
    let bytes = xattr::get(path, HASH_XATTR)
        .map_err(|e| format!("reading {HASH_XATTR} from {}: {e}", path.display()))?
        .ok_or_else(|| format!("no {HASH_XATTR} recorded for {}", path.display()))?;
    String::from_utf8(bytes).map_err(|e| format!("invalid {HASH_XATTR} on {}: {e}", path.display()))
}

/// Fail fast if the CAS directory can't store the `user.*` xattrs we use to
/// record source hashes (some filesystems — tmpfs on older kernels, certain
/// overlay setups — don't support them).
fn probe_xattr(cas: &Path) -> Result<(), String> {
    if !cas.is_dir() {
        return Err(format!("CAS directory {} does not exist", cas.display()));
    }
    xattr::set(cas, PROBE_XATTR, b"1").map_err(|e| {
        format!(
            "{} does not support user extended attributes, which caos needs to \
             record source hashes: {e}",
            cas.display()
        )
    })?;
    let _ = xattr::remove(cas, PROBE_XATTR);
    Ok(())
}

/// Blob → atomically write `data` to `target`, tagged with `hash`.
fn write_file(target: &Path, hash: &str, data: &[u8]) -> Result<(), String> {
    atomically(target, |tmp| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(tmp)
            .map_err(|e| format!("creating {}: {e}", tmp.display()))?;
        file.write_all(data)
            .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        set_hash(tmp, hash.as_bytes())
    })
}

/// Tree → atomically create `target` as a directory tagged with `hash`, holding
/// one empty placeholder per entry (a directory for subtrees, a file otherwise),
/// each tagged with that entry's oid so it can later be expanded with `get`.
fn write_tree(target: &Path, hash: &str, tree: &gix::objs::TreeRef) -> Result<(), String> {
    atomically(target, |tmp| {
        std::fs::create_dir(tmp).map_err(|e| format!("creating {}: {e}", tmp.display()))?;
        set_hash(tmp, hash.as_bytes())?;
        for entry in &tree.entries {
            let child = tmp.join(OsStr::from_bytes(entry.filename));
            if entry.mode.is_tree() {
                std::fs::create_dir(&child)
                    .map_err(|e| format!("creating {}: {e}", child.display()))?;
            } else {
                std::fs::File::create(&child)
                    .map_err(|e| format!("creating {}: {e}", child.display()))?;
            }
            set_hash(&child, entry.oid.to_string().as_bytes())?;
        }
        Ok(())
    })
}

/// Build content at a unique temp sibling of `target` via `build`, then rename
/// it into place atomically; the temp path is cleaned up on any failure.
///
/// The temp lives in the same directory (hence the same filesystem) as
/// `target`, so the final `rename` is atomic — concurrent `caos` processes
/// never see a half-written path or one missing its hash xattr.
fn atomically(
    target: &Path,
    build: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let tmp = temp_path(target)?;
    let result = build(&tmp).and_then(|()| {
        std::fs::rename(&tmp, target)
            .map_err(|e| format!("renaming into place {}: {e}", target.display()))
    });
    if result.is_err() {
        // One of these is a no-op depending on whether `tmp` is a file or dir.
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result
}

/// A unique sibling path of `target` (same directory ⇒ same filesystem).
fn temp_path(target: &Path) -> Result<PathBuf, String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?;
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(".caos-tmp.{pid}.{nanos}.{seq}")))
}

/// Record the source hash of `path` in its `user.caos.hash` xattr.
fn set_hash(path: &Path, hash: &[u8]) -> Result<(), String> {
    xattr::set(path, HASH_XATTR, hash)
        .map_err(|e| format!("setting {HASH_XATTR} on {}: {e}", path.display()))
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
