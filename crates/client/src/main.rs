//! caos: client for the caos server (storage + compute).
//!
//! Subcommands:
//!
//! * `get-hash <hash> <path>` — fetch the git object `<hash>` from the object
//!   server (base URL from `$CAOS_SERVER_URL`) and materialize it at
//!   `<path>`, a direct child of `/cas`: a blob becomes a file holding its
//!   bytes; a tree becomes a directory holding one empty placeholder per entry
//!   (a directory for subtrees, a file otherwise).
//! * `get [-r | --recursive[=<depth>]] <path>` — expand an existing placeholder
//!   anywhere under `/cas`: read its recorded hash, fetch that object, and
//!   replace the empty file with the blob's content, or the empty directory with
//!   the tree's entries. By default it loads one level; `--recursive=<depth>`
//!   loads that many levels and `-r` loads the whole subtree.
//! * `import-image <docker-archive> <cas-path>` — store a docker-archive image
//!   (e.g. `nix build .#caos-worker-hello-docker`) into the CAS in git-docker
//!   form (a tree of `config.json` + `layer<NN>` subtrees), so it can be run with
//!   `run <cas-path>`.
//! * `run <image> <output> -- [--name=value ...]` — assemble the args into a git
//!   tree, ask the server (`$CAOS_SERVER_URL`) to run `<image>`
//!   over it, and materialize the returned result hash at `<output>`. `<image>`
//!   is a CAS path — a git image, resolved to the git hash recorded on it — a
//!   bare git hash, or `docker://<ref>` for an ordinary docker image.
//! * `curry <image> -- [--name=value ...]` — bind some args to `<image>`,
//!   printing a ref to the resulting curried image. The ref runs (supplying the
//!   rest of the args) and curries again just like any image; the binding is
//!   partial application stored as a small CAS tree, not a rebuilt image.
//!
//! Every materialized path is tagged with the git hash it came from in the
//! `user.caos.hash` extended attribute — the top-level path with `<hash>`, and
//! each child of a tree with that entry's own oid. This is both the on-disk,
//! per-path, thread-safe mapping from CAS paths back to hashes, and what lets
//! `get` expand a placeholder later.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gix::objs::WriteTo;

/// Base URL of the caos server (storage + compute), e.g. `http://caos-server`.
const SERVER_ENV: &str = "CAOS_SERVER_URL";

/// The chain of `(image, args)` computations currently in progress, set by the
/// server on each worker it spawns. `caos run` echoes it back so the
/// server can detect a run that re-enters a computation already on the stack
/// (an unresolvable cycle). It rides in env, never the args tree, so the result
/// cache key (image + args) is unaffected.
const RUN_STACK_ENV: &str = "CAOS_RUN_STACK";

/// Image-ref scheme marking an ordinary docker reference (vs. a git-image hash).
const DOCKER_SCHEME: &str = "docker://";

/// Marker entry naming a curry node: a CAS tree that pairs a `base` image ref
/// with an `args` subtree of bound arguments. `run`/`curry` expand it client-side
/// (merging the bound args under the call's args) so the server only ever
/// sees an ordinary image + args hash. The marker lets it be told apart from a
/// git-docker image tree, which it otherwise resembles. See [`unwrap_curry`].
const CURRY_MARKER: &str = ".caos-curry";

/// The program `entrypoint` always runs. Images that build off the
/// `caos-worker-base` image supply this binary.
const DEFAULT_WORKER: &str = "/worker";

/// Directory under which objects are materialized. Override (e.g. for local
/// runs outside the container) with `CAOS_CAS_DIR`.
const CAS_DIR_ENV: &str = "CAOS_CAS_DIR";
const DEFAULT_CAS_DIR: &str = "/cas";

/// xattr recording the git hash a materialized path came from.
const HASH_XATTR: &str = "user.caos.hash";
/// xattr used only by the startup support probe.
const PROBE_XATTR: &str = "user.caos.probe";

/// Permissions for everything under `/cas`. The directory and its contents are
/// owned by root; the worker runs unprivileged and reaches `/cas` only through
/// this (setuid-root) binary, so the modes here decide what the worker may *read*
/// directly — never what it may write (it can't write any of these). Two rules:
///
/// * Fetched content is world-readable: a blob is `r--r--r--`, a tree directory
///   `r-xr-xr-x` plus owner-write so `get`/`put` can fill it. The worker can read
///   what it has loaded but not tamper with it.
/// * A placeholder — a path that exists but hasn't been fetched with `get`/
///   `get-hash` yet — is owner-only (`r--------` / `r-x------`). The worker can't
///   read it by accident, but the owner (root in the container, or the invoking
///   user for a local `CAOS_CAS_DIR` run) can still read the recorded hash to
///   expand it later.
const MODE_FETCHED_FILE: u32 = 0o444;
const MODE_FETCHED_DIR: u32 = 0o755;
const MODE_PLACEHOLDER_FILE: u32 = 0o400;
const MODE_PLACEHOLDER_DIR: u32 = 0o500;

/// The unprivileged user `entrypoint` runs `/worker` as. The container starts as
/// root so `entrypoint` can set up — and later tear down — the root-owned
/// `/cas`; it drops to this uid/gid only for the `/worker` child. The worker
/// therefore can't tamper with `/cas` directly: it must go through `caos`, which
/// is setuid-root. Override (e.g. for a different image user) with the env vars.
const WORKER_UID_ENV: &str = "CAOS_WORKER_UID";
const WORKER_GID_ENV: &str = "CAOS_WORKER_GID";
const DEFAULT_WORKER_UID: u32 = 1000;
const DEFAULT_WORKER_GID: u32 = 1000;

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
        Some("get") => {
            let (path, depth) = parse_get(&args[2..])?;
            get(path, depth)
        }
        Some("put") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(src), Some(dst), None) => put(src, dst),
            _ => Err(usage(args)),
        },
        Some("import-image") => match (args.get(2), args.get(3), args.get(4)) {
            (Some(archive), Some(dst), None) => import_image(archive, dst),
            _ => Err(usage(args)),
        },
        // `run <image> <output> -- [--name=value ...]`. The `--` separates the
        // fixed arguments from the (possibly empty) list of key/value args.
        Some("run") => match &args[2..] {
            [image, output, sep, kvs @ ..] if sep == "--" => caos_run(image, output, kvs),
            _ => Err(usage(args)),
        },
        // `curry <image> -- [--name=value ...]` — bind args to an image, printing
        // a ref to the resulting curried image (run/curry it like any image).
        Some("curry") => match &args[2..] {
            [image, sep, kvs @ ..] if sep == "--" => caos_curry(image, kvs),
            _ => Err(usage(args)),
        },
        // `build-args [--name=value ...]` — print the hash of the assembled args
        // tree (path values stored from disk, everything else a literal blob).
        Some("build-args") => build_args(&args[2..]),
        // `entrypoint [--args=<hash>]` — takes no command; it always runs /worker.
        Some("entrypoint") => match &args[2..] {
            [] => entrypoint(None),
            [flag] => match flag.strip_prefix("--args=") {
                Some(hash) => entrypoint(Some(hash)),
                None => Err(usage(args)),
            },
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
        "usage:\n  {prog} get-hash <hash> <path>\n  \
         {prog} get [-r | --recursive[=<depth>]] <path>\n  \
         {prog} put <src-path> <cas-path>\n  \
         {prog} import-image <docker-archive> <cas-path>\n  \
         {prog} run <image> <output-cas-path> -- [--name=value ...]\n  \
         {prog} curry <image> -- [--name=value ...]\n  \
         {prog} build-args [--name=value ...]\n  \
         {prog} entrypoint [--args=<hash>]"
    )
}

/// `get-hash <hash> <path>` — fetch `<hash>` and materialize it at `<path>`,
/// which must be a direct child of the CAS directory.
fn get_hash(hash: &str, path: &str) -> Result<(), String> {
    let base = server_url()?;
    let cas = cas_dir();
    let target = validate_target(&cas, path)?;
    probe_xattr(&cas)?;
    fetch_and_materialize(&base, &target, hash)
}

/// `get [-r | --recursive[=<depth>]] <path>` — re-materialize the object recorded
/// at `<path>` (a path inside the CAS directory, possibly deep). Reads `<path>`'s
/// recorded hash, fetches that object, and replaces the placeholder: an empty
/// file with the blob's content, or an empty directory with the tree's entries.
///
/// `depth` counts how many levels to load: the default (a plain `get`) loads one
/// — `<path>` itself, leaving a tree's entries as placeholders — while
/// `--recursive=<n>` loads `n` levels and `-r` (or bare `--recursive`) loads the
/// whole subtree.
fn get(path: &str, depth: Option<u32>) -> Result<(), String> {
    let base = server_url()?;
    let cas = cas_dir();
    let target = validate_descendant(&cas, path)?;
    probe_xattr(&cas)?;
    expand(&base, &target, depth)
}

/// Materialize the placeholder at `target` from its recorded hash, then — if it
/// became a directory and `depth` allows another level — expand each child the
/// same way. `depth` is the number of levels left to load: `Some(1)` stops after
/// `target` (a plain `get`), `Some(n)` descends `n - 1` more levels, and `None`
/// loads the whole subtree. (A git object graph is a finite DAG, so unbounded
/// recursion always terminates at the blobs.)
fn expand(base: &str, target: &Path, depth: Option<u32>) -> Result<(), String> {
    // Fetch only an unexpanded placeholder. An already-loaded node is left as is
    // and we just descend into it, so `get -r` is idempotent and can finish
    // loading a tree that was already partially expanded (e.g. after `get-hash`).
    // Re-fetching here would also fail anyway: renaming the fresh copy over a
    // non-empty directory is `ENOTEMPTY`.
    if !is_loaded(target) {
        let hash = read_hash(target)?;
        fetch_and_materialize(base, target, &hash)?;
    }

    let child_depth = match depth {
        Some(1) => return Ok(()), // this was the last level to load
        Some(n) => Some(n - 1),
        None => None, // unbounded
    };

    // A tree just got materialized as a directory of child placeholders. Collect
    // them before recursing: expanding a child renames a temp sibling into this
    // same directory, so we must finish reading it first.
    if target.is_dir() {
        let mut children = Vec::new();
        for entry in
            std::fs::read_dir(target).map_err(|e| format!("reading {}: {e}", target.display()))?
        {
            let entry = entry.map_err(|e| format!("reading {}: {e}", target.display()))?;
            children.push(entry.path());
        }
        for child in children {
            expand(base, &child, child_depth)?;
        }
    }
    Ok(())
}

/// Whether `path` has already been fetched, as opposed to an unexpanded
/// placeholder. Loaded content is group/other-readable; a placeholder is
/// owner-only (see `MODE_FETCHED_*` vs `MODE_PLACEHOLDER_*`), so the read bits
/// double as the "is this loaded yet?" marker.
fn is_loaded(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.permissions().mode() & 0o044 != 0)
        .unwrap_or(false)
}

/// Parse `get`'s arguments: an optional recursion flag plus exactly one path.
/// `-r` and bare `--recursive` mean the whole subtree (`None`); `--recursive=<n>`
/// means `n` levels (`n >= 1`); absent, the default is one level (`Some(1)`).
fn parse_get(args: &[String]) -> Result<(&str, Option<u32>), String> {
    let mut path: Option<&str> = None;
    let mut depth = Some(1);
    for arg in args {
        if arg == "-r" || arg == "--recursive" {
            depth = None;
        } else if let Some(n) = arg.strip_prefix("--recursive=") {
            let n: u32 = n
                .parse()
                .map_err(|_| format!("recursion depth must be a number, got: {n:?}"))?;
            if n < 1 {
                return Err("recursion depth must be at least 1".to_string());
            }
            depth = Some(n);
        } else if arg.starts_with('-') && arg != "-" {
            return Err(format!("unknown option for get: {arg}"));
        } else if path.is_none() {
            path = Some(arg);
        } else {
            return Err(format!("get takes a single path, got an extra: {arg}"));
        }
    }
    let path = path.ok_or_else(|| "get requires a path".to_string())?;
    Ok((path, depth))
}

/// `entrypoint [--args=<hash>]` — the container entrypoint. Wipes the CAS
/// directory, optionally populates `/cas/args` from `--args=<hash>`, runs
/// `/worker`, prints the hash recorded at `/cas/out`, then removes the CAS
/// directory.
fn entrypoint(args_hash: Option<&str>) -> Result<(), String> {
    let cas = cas_dir();

    // Start clean: delete the CAS directory and recreate it empty (fail if we
    // can't), then verify it supports the xattrs we rely on.
    remove_cas(&cas)?;
    std::fs::create_dir_all(&cas).map_err(|e| format!("creating {}: {e}", cas.display()))?;
    // Root-owned and only root-writable: the worker reaches `/cas` solely through
    // this setuid-root binary, never by writing here directly.
    set_mode(&cas, MODE_FETCHED_DIR)?;
    probe_xattr(&cas)?;

    // Populate /cas/args from the given hash, like `get-hash <hash> /cas/args`,
    // so the worker can read its inputs there.
    if let Some(hash) = args_hash {
        let base = server_url()?;
        fetch_and_materialize(&base, &cas.join("args"), hash)?;
    }

    // Run the worker, sending its stdout to our stderr so that our own stdout
    // carries only the resulting hash. We stay root (to tear down `/cas` after),
    // but drop the *worker* to an unprivileged user so it can't tamper with the
    // root-owned `/cas` — only the setuid-root `caos` it invokes can.
    let stdout = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("duplicating stderr: {e}"))?;
    let uid = env_u32(WORKER_UID_ENV).unwrap_or(DEFAULT_WORKER_UID);
    let gid = env_u32(WORKER_GID_ENV).unwrap_or(DEFAULT_WORKER_GID);
    let mut command = std::process::Command::new(DEFAULT_WORKER);
    command.stdout(std::process::Stdio::from(stdout));
    // SAFETY: the closure runs in the forked child before exec and only makes
    // async-signal-safe syscalls. We drop privileges by hand (rather than
    // `Command::uid`/`gid`) so we can also clear supplementary groups — `groups`
    // is still unstable — and in the right order: groups, then gid, then uid,
    // each while we're still root.
    unsafe {
        command.pre_exec(move || {
            if drop_privileges(uid, gid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let status = command
        .status()
        .map_err(|e| format!("running {DEFAULT_WORKER}: {e}"))?;
    if !status.success() {
        return Err(format!("{DEFAULT_WORKER} exited with {status}"));
    }

    // Everything under /cas got there via get/put, which tag each path with its
    // hash, so /cas/out already knows its hash — read it back before teardown.
    let hash = read_hash(&cas.join("out"))?;

    // Tear down.
    remove_cas(&cas)?;

    println!("{hash}");
    Ok(())
}

/// Delete the CAS directory and everything in it. Succeeds if it's already gone.
fn remove_cas(cas: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(cas) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("removing {}: {e}", cas.display())),
    }
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

/// Base URL of the caos server (storage + compute), from [`SERVER_ENV`].
fn server_url() -> Result<String, String> {
    std::env::var(SERVER_ENV)
        .map_err(|_| format!("{SERVER_ENV} must be set to the caos server URL"))
}

/// `put <src-path> <cas-path>` — recursively store `<src-path>` (a path outside
/// the CAS) into the server and record the result at `<cas-path>`, a
/// direct child of the CAS directory.
///
/// Files are stored as blobs and directories as trees — both as real git objects
/// (the server writes trees with `write_object`, so their hashes are genuine git
/// tree hashes). A symlink that resolves to something already in the CAS is *not*
/// re-read — its recorded hash is reused, so shared content is stored once.
fn put(src: &str, dst: &str) -> Result<(), String> {
    let base = server_url()?;
    let cas = cas_dir();
    let target = validate_target(&cas, dst)?;
    probe_xattr(&cas)?;
    let cas_real = cas
        .canonicalize()
        .map_err(|e| format!("CAS directory {}: {e}", cas.display()))?;

    let (_, oid) = store(&base, &cas_real, Path::new(src))?;
    fetch_and_materialize(&base, &target, &oid.to_string())
}

/// `import-image <docker-archive> <cas-path>` — store a docker-archive image (the
/// kind `nix build .#caos-*-docker` / `docker save` produce) into the CAS in
/// git-docker form: a tree holding `config.json` (the image config, verbatim) and
/// one `layer<NN>` subtree per layer (the layer tar's extracted filesystem),
/// materialized at `<cas-path>`. `run <cas-path>` then has the server
/// convert it back into a real image.
///
/// Only the layer *contents* are captured (files, the exec bit, and symlinks);
/// mtimes/owners are dropped, which is fine — the server re-tars the trees
/// deterministically and generates the diff_ids itself.
fn import_image(archive: &str, dst: &str) -> Result<(), String> {
    use gix::objs::tree::{Entry, EntryKind};

    let base = server_url()?;
    let cas = cas_dir();
    let target = validate_target(&cas, dst)?;
    probe_xattr(&cas)?;
    let cas_real = cas
        .canonicalize()
        .map_err(|e| format!("CAS directory {}: {e}", cas.display()))?;

    let work = scratch_dir()?;
    let outcome = (|| {
        // Unpack the (possibly gzipped) outer archive into the scratch dir.
        let bytes = maybe_gunzip(std::fs::read(archive).map_err(|e| format!("{archive}: {e}"))?)?;
        unpack_tar(&bytes, &work)?;

        // manifest.json names the config blob and the ordered layers.
        let manifest_bytes = std::fs::read(work.join("manifest.json"))
            .map_err(|e| format!("reading manifest.json from {archive}: {e}"))?;
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| format!("parsing manifest.json: {e}"))?;
        let image = manifest.get(0).ok_or("manifest.json is empty")?;
        let config_name = image
            .get("Config")
            .and_then(|v| v.as_str())
            .ok_or("manifest.json: missing string Config")?;
        let layers = image
            .get("Layers")
            .and_then(|v| v.as_array())
            .ok_or("manifest.json: missing Layers array")?;

        let mut entries: Vec<Entry> = Vec::new();

        // config.json, stored verbatim.
        let config_bytes = std::fs::read(work.join(config_name))
            .map_err(|e| format!("reading {config_name}: {e}"))?;
        entries.push(Entry {
            mode: EntryKind::Blob.into(),
            filename: "config.json".as_bytes().to_vec().into(),
            oid: post_object(&base, "blob", &config_bytes)?,
        });

        // layer<NN>: one subtree per layer, in manifest order.
        for (i, layer) in layers.iter().enumerate() {
            let layer_path = layer
                .as_str()
                .ok_or("manifest.json: Layers entry is not a string")?;
            let layer_bytes = maybe_gunzip(
                std::fs::read(work.join(layer_path))
                    .map_err(|e| format!("reading {layer_path}: {e}"))?,
            )?;
            let layer_dir = work.join(format!("extract-layer{i:02}"));
            std::fs::create_dir(&layer_dir).map_err(|e| format!("{}: {e}", layer_dir.display()))?;
            unpack_tar(&layer_bytes, &layer_dir)?;
            // Record perms/ownership a git tree can't carry, as sidecars beside
            // each entry, before storing the layer as a tree.
            write_layer_metadata(&layer_bytes, &layer_dir)?;
            let (_, oid) = store(&base, &cas_real, &layer_dir)?;
            entries.push(Entry {
                mode: EntryKind::Tree.into(),
                filename: format!("layer{i:02}").into_bytes().into(),
                oid,
            });
            eprintln!("imported layer{i:02} from {layer_path}");
        }

        let image_oid = post_tree(&base, entries)?;
        fetch_and_materialize(&base, &target, &image_oid.to_string())
    })();

    let _ = std::fs::remove_dir_all(&work);
    outcome
}

/// Reserved suffix for the per-entry permission sidecars (see [`write_layer_metadata`]).
const META_SUFFIX: &str = ".caosmeta";

/// Beside any entry in the already-unpacked layer at `dir` whose permissions or
/// ownership a git tree can't reproduce, write a `<name>.caosmeta` sidecar — a
/// small JSON `{"mode":"<octal>","uid":N,"gid":N}` — so the server can
/// restore them when it rebuilds the layer's tar. Files and directories are
/// treated alike: the sidecar sits next to the entry, in its parent.
///
/// Metadata comes from the layer **tar headers**, not from the unpacked files:
/// the headers are authoritative, whereas the unpacked owner/mode depend on who
/// ran the unpack (a non-root unpack can't reproduce a non-root owner).
///
/// "Can't reproduce" means the entry's bits differ from what a plain materialize
/// would recreate: a directory not `0755`, a file not `0644`/`0755` (so setuid,
/// setgid, sticky, and odd perms are all captured), or non-root owner/group. Only
/// regular files and directories are recorded; symlinks, hardlinks, and device
/// nodes are skipped. Errors if the layer itself already uses the reserved suffix
/// (we'd otherwise shadow a real file).
fn write_layer_metadata(layer_tar: &[u8], dir: &Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(layer_tar);
    for entry in archive
        .entries()
        .map_err(|e| format!("reading layer tar: {e}"))?
    {
        let entry = entry.map_err(|e| format!("reading layer tar: {e}"))?;
        let header = entry.header();
        let is_dir = header.entry_type().is_dir();
        // Only plain files and directories carry perms we record here.
        if !is_dir && !header.entry_type().is_file() {
            continue;
        }
        let mode = header.mode().map_err(|e| format!("layer tar mode: {e}"))? & 0o7777;
        let uid = header.uid().map_err(|e| format!("layer tar uid: {e}"))?;
        let gid = header.gid().map_err(|e| format!("layer tar gid: {e}"))?;

        let rel = normalize_tar_path(&entry.path().map_err(|e| format!("layer tar path: {e}"))?);
        if rel.as_os_str().is_empty() {
            continue; // the layer root (".") — no parent to hold a sidecar
        }
        if rel.to_string_lossy().ends_with(META_SUFFIX) {
            return Err(format!(
                "layer uses the reserved {META_SUFFIX} suffix: {}",
                rel.display()
            ));
        }

        let default = if is_dir || mode & 0o111 != 0 {
            0o755
        } else {
            0o644
        };
        if mode == default && uid == 0 && gid == 0 {
            continue;
        }

        // Drop the sidecar next to the (already unpacked) entry. Its parent may be
        // a read-only nix store dir, so make it writable first — harmless, since a
        // git tree records no directory mode and the parent's own mode rides in
        // its own sidecar.
        let entry_path = dir.join(&rel);
        let parent = entry_path.parent().unwrap_or(dir);
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", parent.display()))?;
        let name = entry_path
            .file_name()
            .ok_or_else(|| format!("layer entry has no name: {}", rel.display()))?
            .to_string_lossy();
        let sidecar = parent.join(format!("{name}{META_SUFFIX}"));
        let json = serde_json::json!({ "mode": format!("{mode:04o}"), "uid": uid, "gid": gid });
        let bytes = serde_json::to_vec(&json).map_err(|e| format!("encoding metadata: {e}"))?;
        std::fs::write(&sidecar, bytes).map_err(|e| format!("{}: {e}", sidecar.display()))?;
    }
    Ok(())
}

/// A tar entry path reduced to its normal components (drops a leading `./` and
/// any trailing slash), so it lines up with the unpacked path under the layer dir.
fn normalize_tar_path(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .collect()
}

/// Decompress `bytes` if it's gzip (magic `1f 8b`); otherwise return it as-is.
/// Image archives are gzipped; the layer tars inside usually aren't.
fn maybe_gunzip(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_slice())
            .read_to_end(&mut out)
            .map_err(|e| format!("gunzip: {e}"))?;
        Ok(out)
    } else {
        Ok(bytes)
    }
}

/// Unpack a tar archive into `dir`, preserving permissions so the exec bit on
/// layer files survives into the git tree.
fn unpack_tar(bytes: &[u8], dir: &Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(bytes);
    archive.set_preserve_permissions(true);
    archive
        .unpack(dir)
        .map_err(|e| format!("unpacking tar into {}: {e}", dir.display()))
}

/// A fresh, unique scratch directory under the system temp dir (no xattrs needed
/// — only the final CAS path is tagged).
fn scratch_dir() -> Result<PathBuf, String> {
    let base = std::env::temp_dir().join("caos-import");
    std::fs::create_dir_all(&base).map_err(|e| format!("creating {}: {e}", base.display()))?;
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!("{pid}.{nanos}.{seq}"));
    std::fs::create_dir(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Recursively store `path` in the server, returning the git tree entry
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
                return cas_entry(&canon);
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
        let oid = post_tree(base, entries)?;
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

/// Tree entry referencing an existing CAS object at `canon` (already
/// canonicalized and known to be inside the CAS root): reuse the hash recorded
/// there rather than re-reading content, with the mode following whether it's a
/// directory. Shared by `store` (symlinks into the CAS) and `build_args_tree`
/// (CAS-path arg values).
fn cas_entry(canon: &Path) -> Result<(gix::objs::tree::EntryMode, gix::ObjectId), String> {
    use gix::objs::tree::EntryKind;
    let kind = if canon.is_dir() {
        EntryKind::Tree
    } else {
        EntryKind::Blob
    };
    Ok((kind.into(), parse_oid(&read_hash(canon)?)?))
}

/// Encode `entries` as a git tree object, POST it to the server, and
/// return its hash. Shared by `store` (real directories) and `build_args_tree`
/// (the synthesized args tree).
fn post_tree(
    base: &str,
    mut entries: Vec<gix::objs::tree::Entry>,
) -> Result<gix::ObjectId, String> {
    // Git requires tree entries in a specific order; Entry's Ord implements it.
    entries.sort();
    let mut buf = Vec::new();
    gix::objs::Tree { entries }
        .write_to(&mut buf)
        .map_err(|e| format!("encoding tree: {e}"))?;
    post_object(base, "tree", &buf)
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

/// `run <image> <output> -- [--name=value ...]` — assemble the args into a git
/// tree, ask the server to run `<image>` over that tree, and materialize
/// the result at `<output>` (a direct child of the CAS directory).
fn caos_run(image: &str, output: &str, kvs: &[String]) -> Result<(), String> {
    let base = server_url()?;
    let compute = server_url()?;
    let cas = cas_dir();
    let target = validate_target(&cas, output)?;
    probe_xattr(&cas)?;

    // Resolve the image: a CAS path becomes the git hash recorded on it, so the
    // server converts it from our git-docker form; a `docker://` ref or a
    // bare hash is sent through unchanged.
    let image = resolve_run_image(&cas, image)?;

    // Expand any curry layers: pull the underlying image out and collect the args
    // bound into it, so the server only ever sees a plain image + args.
    let (image, bound) = unwrap_curry(&base, &image)?;

    // Build the call's args, then merge them over the bound ones (call wins).
    // Nothing is written under /cas — the worker materializes the tree itself.
    let call = build_arg_entries(&base, &cas, kvs)?;
    let args_tree = post_tree(&base, merge_entries(bound, call))?;

    // Hand the image and args-tree hash to the server; it runs the
    // container and returns the hash of the result (its /cas/out).
    let result = request_compute(&compute, &image, &args_tree.to_string())?;

    // Materialize that result at the requested output path.
    fetch_and_materialize(&base, &target, &result)
}

/// Resolve the `<image>` argument of `caos run` into what the server
/// expects. A git image is given as a path inside the CAS, which resolves to the
/// git hash recorded on it; a `docker://<ref>` value is an ordinary docker image
/// and passes through unchanged. Anything else is rejected.
fn resolve_run_image(cas: &Path, image: &str) -> Result<String, String> {
    if image.starts_with(DOCKER_SCHEME) {
        return Ok(image.to_string());
    }
    // A bare git hash — a git image or a curry node already in the store, e.g. a
    // ref produced by `caos curry`. Location-independent, so it survives being
    // passed through args into a worker (a CAS path would not). Sent as-is.
    if is_hex_hash(image) {
        return Ok(image.to_string());
    }
    // A path inside the CAS: reference whatever git object it was made from.
    if Path::new(image).starts_with(cas) {
        let canon = Path::new(image)
            .canonicalize()
            .map_err(|e| format!("{image}: {e}"))?;
        let cas_real = cas
            .canonicalize()
            .map_err(|e| format!("CAS directory {}: {e}", cas.display()))?;
        if !canon.starts_with(&cas_real) {
            return Err(format!("{image} resolves outside {}", cas.display()));
        }
        return read_hash(&canon);
    }
    Err(format!(
        "image must be a path under {} (a git image), a git hash, or \
         {DOCKER_SCHEME}<ref>, got: {image}",
        cas.display()
    ))
}

/// A bare 40-char SHA-1 hash, naming a git object directly (a git image or a
/// curry node). Length-checked so a short CAS-relative path isn't mistaken for
/// one.
fn is_hex_hash(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `curry <image> -- [--name=value ...]` — bind arguments to `<image>`, printing
/// a ref (a git hash) to the resulting curried image. The ref can be `run` —
/// which supplies the rest of the args — or `curry`'d again, exactly like any
/// image; the binding is partial application, not a rebuilt container image.
///
/// The curried image is a small CAS tree: a `base` blob (the underlying image
/// ref), an `args` subtree (the bound args, in `build_arg_entries` shape), and a
/// [`CURRY_MARKER`] blob. Currying flattens: if `<image>` is itself curried, its
/// bindings are folded in and `base` stays a plain (docker/git) image, so the
/// result is canonical (`curry (curry img a) b` == `curry img a b`).
fn caos_curry(image: &str, kvs: &[String]) -> Result<(), String> {
    use gix::objs::tree::{Entry, EntryKind};

    let base = server_url()?;
    let cas = cas_dir();

    let image = resolve_run_image(&cas, image)?;
    let (image, bound) = unwrap_curry(&base, &image)?;

    // New bindings override any already bound to the same name.
    let args = merge_entries(bound, build_arg_entries(&base, &cas, kvs)?);
    let args_tree = post_tree(&base, args)?;

    let entries = vec![
        Entry {
            mode: EntryKind::Blob.into(),
            filename: b"base".to_vec().into(),
            oid: post_object(&base, "blob", image.as_bytes())?,
        },
        Entry {
            mode: EntryKind::Tree.into(),
            filename: b"args".to_vec().into(),
            oid: args_tree,
        },
        Entry {
            mode: EntryKind::Blob.into(),
            filename: CURRY_MARKER.as_bytes().to_vec().into(),
            oid: post_object(&base, "blob", b"1")?,
        },
    ];
    println!("{}", post_tree(&base, entries)?);
    Ok(())
}

/// Peel any curry layers off `image` (a resolved ref: `docker://…` or a git
/// hash), returning the underlying plain image and the args bound into it. A
/// caller merges these *under* its own args, so call-time args win; with curry's
/// flattening there is normally a single layer, but nested layers are handled
/// defensively (an outer binding wins over an inner one for the same name).
fn unwrap_curry(base: &str, image: &str) -> Result<(String, Vec<gix::objs::tree::Entry>), String> {
    let mut image = image.to_string();
    let mut bound = Vec::new();
    while is_hex_hash(&image) {
        match curry_node(base, &image)? {
            None => break, // a plain git image, not a curry node
            Some((inner_image, inner_args)) => {
                // `bound` holds outer layers, which win over this deeper one.
                bound = merge_entries(inner_args, bound);
                image = inner_image;
            }
        }
    }
    Ok((image, bound))
}

/// If `hash` names a curry node, return its base image ref and bound-args
/// entries; otherwise `None` (a blob, or a tree without the [`CURRY_MARKER`] —
/// e.g. a git-docker image).
fn curry_node(
    base: &str,
    hash: &str,
) -> Result<Option<(String, Vec<gix::objs::tree::Entry>)>, String> {
    let entries = match fetch_tree_entries(base, hash)? {
        Some(entries) => entries,
        None => return Ok(None),
    };
    if !entries.iter().any(|e| entry_name(e) == CURRY_MARKER.as_bytes()) {
        return Ok(None);
    }
    let oid_of = |name: &[u8]| {
        entries
            .iter()
            .find(|e| entry_name(e) == name)
            .map(|e| e.oid)
            .ok_or_else(|| format!("curry node {hash} missing {:?}", String::from_utf8_lossy(name)))
    };
    let base_ref = fetch_blob_string(base, &oid_of(b"base")?.to_string())?;
    let args = fetch_tree_entries(base, &oid_of(b"args")?.to_string())?
        .ok_or_else(|| format!("curry node {hash} 'args' is not a tree"))?;
    Ok(Some((base_ref, args)))
}

/// Fetch object `hash`; if it's a tree, return its entries as owned values, else
/// `None`.
fn fetch_tree_entries(
    base: &str,
    hash: &str,
) -> Result<Option<Vec<gix::objs::tree::Entry>>, String> {
    let url = format!("{}/object/{hash}", base.trim_end_matches('/'));
    let serialized = http_get(&url)?;
    let (kind, content) = parse_object(&serialized)?;
    if kind != "tree" {
        return Ok(None);
    }
    let tree = gix::objs::TreeRef::from_bytes(content, gix::hash::Kind::Sha1)
        .map_err(|e| format!("malformed tree {hash}: {e}"))?;
    Ok(Some(
        tree.entries
            .iter()
            .map(|e| gix::objs::tree::Entry {
                mode: e.mode,
                filename: e.filename.to_vec().into(),
                oid: e.oid.to_owned(),
            })
            .collect(),
    ))
}

/// Fetch blob `hash` as a trimmed UTF-8 string.
fn fetch_blob_string(base: &str, hash: &str) -> Result<String, String> {
    let url = format!("{}/object/{hash}", base.trim_end_matches('/'));
    let serialized = http_get(&url)?;
    let (kind, content) = parse_object(&serialized)?;
    if kind != "blob" {
        return Err(format!("expected a blob at {hash}, got {kind}"));
    }
    let text = std::str::from_utf8(content).map_err(|e| format!("blob {hash} not UTF-8: {e}"))?;
    Ok(text.trim().to_string())
}

/// A tree entry's filename as raw bytes (pins the `AsRef` impl `BString` offers).
fn entry_name(e: &gix::objs::tree::Entry) -> &[u8] {
    e.filename.as_ref()
}

/// Merge two sets of tree entries by filename; entries in `high` override those
/// in `low`. Order is irrelevant — `post_tree` sorts before encoding.
fn merge_entries(
    low: Vec<gix::objs::tree::Entry>,
    high: Vec<gix::objs::tree::Entry>,
) -> Vec<gix::objs::tree::Entry> {
    let mut by_name = std::collections::BTreeMap::new();
    for e in low.into_iter().chain(high) {
        by_name.insert(e.filename.to_vec(), e);
    }
    by_name.into_values().collect()
}

/// Split a `--name=value` argument into its name and value, validating that the
/// name is a single path component (it becomes a tree-entry filename).
fn parse_kv(kv: &str) -> Result<(&str, &str), String> {
    let body = kv
        .strip_prefix("--")
        .ok_or_else(|| format!("argument must look like --name=value, got: {kv}"))?;
    let (name, value) = body
        .split_once('=')
        .ok_or_else(|| format!("argument must look like --name=value, got: {kv}"))?;
    if name.is_empty() || name.contains('/') {
        return Err(format!(
            "argument name must be a single path component, got: {name:?}"
        ));
    }
    Ok((name, value))
}

/// `build-args [--name=value ...]` — assemble an args git tree and print its
/// hash. For each pair, a `value` that names an existing path (relative to the
/// working directory) is stored recursively from the filesystem, so the entry
/// references that content's own hash (git's hash for the path); any other value
/// is stored verbatim as a blob. The printed hash is meant to be passed to
/// `caos entrypoint --args=<hash>` (which is what `run-worker-bash.sh` does).
fn build_args(kvs: &[String]) -> Result<(), String> {
    let base = server_url()?;
    let hash = build_host_args_tree(&base, kvs)?;
    println!("{hash}");
    Ok(())
}

/// The args-tree builder behind `build-args`: like [`build_args_tree`], but a
/// `value` naming an existing filesystem path is stored from disk (reusing
/// [`store`]) instead of being read as a CAS path. The tree is never written to
/// the filesystem.
fn build_host_args_tree(base: &str, kvs: &[String]) -> Result<gix::ObjectId, String> {
    use gix::objs::tree::{Entry, EntryKind};

    // There's no CAS here, so `store` should never treat a symlink as one
    // pointing into a CAS. A NUL byte can't appear in a real (canonicalized)
    // path, so no path ever starts with this sentinel — the CAS-reuse branch
    // stays dormant and symlinks are stored as plain git symlinks.
    let no_cas = PathBuf::from("/\0");

    let mut entries = Vec::new();
    for kv in kvs {
        let (name, value) = parse_kv(kv)?;

        let (mode, oid) = if Path::new(value).exists() {
            store(base, &no_cas, Path::new(value))?
        } else {
            // Not a path: store the literal value as a blob.
            (
                EntryKind::Blob.into(),
                post_object(base, "blob", value.as_bytes())?,
            )
        };

        entries.push(Entry {
            mode,
            filename: name.as_bytes().to_vec().into(),
            oid,
        });
    }

    post_tree(base, entries)
}

/// The per-`--name=value` tree entries that make up an args tree — `run`/`curry`
/// merge call args with a curry node's bound args, then `post_tree` the result.
///
/// Each `--name=value` becomes a tree entry `name`:
/// * if `value` is a path inside the CAS directory, it must exist, and the entry
///   references the object that path was materialized from (its recorded hash);
/// * otherwise `value` is stored verbatim as a blob and the entry references it.
fn build_arg_entries(
    base: &str,
    cas: &Path,
    kvs: &[String],
) -> Result<Vec<gix::objs::tree::Entry>, String> {
    use gix::objs::tree::{Entry, EntryKind};

    // Canonical CAS root, resolved lazily — only needed if a CAS path appears.
    let cas_real = cas.canonicalize();

    let mut entries = Vec::new();
    for kv in kvs {
        let (name, value) = parse_kv(kv)?;

        let (mode, oid) = if Path::new(value).starts_with(cas) {
            // A CAS path: it must exist; reference whatever it was made from.
            let canon = Path::new(value)
                .canonicalize()
                .map_err(|e| format!("{value}: {e}"))?;
            let cas_real = cas_real
                .as_ref()
                .map_err(|e| format!("CAS directory {}: {e}", cas.display()))?;
            if !canon.starts_with(cas_real) {
                return Err(format!("{value} resolves outside {}", cas.display()));
            }
            cas_entry(&canon)?
        } else {
            // A literal value: store it as a blob and reference that.
            (
                EntryKind::Blob.into(),
                post_object(base, "blob", value.as_bytes())?,
            )
        };

        entries.push(Entry {
            mode,
            filename: name.as_bytes().to_vec().into(),
            oid,
        });
    }

    Ok(entries)
}

/// Ask the server to run `image` over the args tree `args_hash`,
/// returning the result hash it prints (the container's /cas/out).
fn request_compute(base: &str, image: &str, args_hash: &str) -> Result<String, String> {
    let mut url = format!(
        "{}/run?image={}&args={}",
        base.trim_end_matches('/'),
        percent_encode(image),
        args_hash,
    );
    // Echo back the in-progress run stack (see RUN_STACK_ENV) so the compute
    // server can detect cycles. Empty/unset at the top level.
    if let Ok(stack) = std::env::var(RUN_STACK_ENV) {
        if !stack.is_empty() {
            url.push_str("&stack=");
            url.push_str(&percent_encode(&stack));
        }
    }
    let body = http_get(&url)?;
    let text = String::from_utf8(body)
        .map_err(|e| format!("server returned invalid UTF-8: {e}"))?;
    let hash = text.trim();
    if hash.is_empty() {
        return Err("server returned an empty result".to_string());
    }
    Ok(hash.to_string())
}

/// Percent-encode a string for use as a URL query value: unreserved characters
/// pass through, everything else becomes `%XX`.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
        set_hash(tmp, hash.as_bytes())?;
        // Fetched blob: world-readable, writable by no one.
        set_mode(tmp, MODE_FETCHED_FILE)
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
            // Each child is a placeholder: it records its hash but holds no
            // content until expanded with `get`, so it stays owner-only — the
            // worker mustn't read what it hasn't fetched.
            let placeholder_mode = if entry.mode.is_tree() {
                std::fs::create_dir(&child)
                    .map_err(|e| format!("creating {}: {e}", child.display()))?;
                MODE_PLACEHOLDER_DIR
            } else {
                std::fs::File::create(&child)
                    .map_err(|e| format!("creating {}: {e}", child.display()))?;
                MODE_PLACEHOLDER_FILE
            };
            set_hash(&child, entry.oid.to_string().as_bytes())?;
            set_mode(&child, placeholder_mode)?;
        }
        // The tree itself *was* fetched (its entries are now visible), so make it
        // readable and traversable. Last, so creating the children above — which
        // needs write on this dir — isn't blocked.
        set_mode(tmp, MODE_FETCHED_DIR)
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

/// Set `path`'s permission bits. Always done *after* the hash xattr is recorded,
/// since a read-only mode would otherwise stop a non-root owner from setting it.
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("setting mode on {}: {e}", path.display()))
}

/// Parse `key` from the environment as a `u32`, or `None` if unset/unparseable.
fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// Drop to `uid`/`gid`, clearing supplementary groups first. Returns 0 on
/// success, or a non-zero return from the first failing syscall (the caller then
/// reads `errno`). Must be called while still privileged, in this order:
/// supplementary groups, then the group, then the user — once the uid is dropped
/// the others would be denied. Only used from `entrypoint`'s `pre_exec`, so it
/// must stay async-signal-safe: these three raw syscalls are.
fn drop_privileges(uid: u32, gid: u32) -> i32 {
    // Resolved against the libc std already links (musl in the image).
    extern "C" {
        fn setgroups(size: usize, list: *const u32) -> i32;
        fn setgid(gid: u32) -> i32;
        fn setuid(uid: u32) -> i32;
    }
    unsafe {
        let rc = setgroups(0, std::ptr::null());
        if rc != 0 {
            return rc;
        }
        let rc = setgid(gid);
        if rc != 0 {
            return rc;
        }
        setuid(uid)
    }
}

/// HTTP GET returning the raw response body. Non-2xx responses are errors.
fn http_get(url: &str) -> Result<Vec<u8>, String> {
    let response = minreq::get(url)
        .send()
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        // Surface the server's response body — for the server a 500
        // carries the worker's failure output, which is what you actually need.
        let body = response.as_str().unwrap_or("").trim();
        let detail = if body.is_empty() {
            String::new()
        } else {
            format!(":\n{body}")
        };
        return Err(format!(
            "GET {url}: server returned {} {}{detail}",
            response.status_code, response.reason_phrase
        ));
    }
    Ok(response.into_bytes())
}
