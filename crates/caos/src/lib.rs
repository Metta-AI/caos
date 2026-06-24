//! caos client library: the logic shared by the two `caos` binaries.
//!
//! There are two clients (see the crate's `bin/`):
//!
//! * **`caos`** — the worker-side client baked setuid-root into worker images.
//!   It talks to the server over HTTP (`/object`, `/run`) and runs the container
//!   `entrypoint`.
//! * **`caos-cli`** — the user-facing client. It uses the server as a `caos` git
//!   remote, building objects in the local working repo and exchanging them with
//!   the server by negotiated push/fetch.
//!
//! Everything that doesn't depend on *how* objects move — the object model,
//! currying, args-tree assembly, CAS materialization, image import — lives here,
//! written against the [`Transport`] trait. Each binary picks a transport
//! ([`HttpTransport`] for the worker; the git remote for the CLI) and calls the
//! command functions below.
//!
//! Every materialized path is tagged with the git hash it came from in the
//! `user.caos.hash` extended attribute — the top-level path with `<hash>`, and
//! each child of a tree with that entry's own oid. This is both the on-disk,
//! per-path, thread-safe mapping from CAS paths back to hashes, and what lets
//! `get` expand a placeholder later.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gix::objs::WriteTo;

/// Base URL of the caos server (storage + compute), e.g. `http://caos-server`.
pub const SERVER_ENV: &str = "CAOS_SERVER_URL";

/// The chain of `(image, args)` computations currently in progress, set by the
/// server on each worker it spawns. `caos run` echoes it back so the
/// server can detect a run that re-enters a computation already on the stack
/// (an unresolvable cycle). It rides in env, never the args tree, so the result
/// cache key (image + args) is unaffected.
pub const RUN_STACK_ENV: &str = "CAOS_RUN_STACK";

/// The built-in tree hash (`std`) in effect for this run. The server sets it on
/// each worker it spawns (materialized at `/cas/std`); `caos run` echoes it back
/// so it threads down the whole tree. At the top it's unset, and the ref named by
/// [`STD_REF_ENV`] is resolved instead. Unlike the run stack, `std` *is* part of
/// the result cache key (it names the standard library a worker can reach).
pub const STD_ENV: &str = "CAOS_STD";
/// Ref resolved to `std` at the top of a run (overridable). Default
/// `refs/caos/std`, read from the local repo.
pub const STD_REF_ENV: &str = "CAOS_STD_REF";
pub const DEFAULT_STD_REF: &str = "refs/caos/std";

/// An opaque cache-busting value mixed into every run's request — and so into its
/// `reqHash` and cache key. Empty by default, so runs are cached purely by their
/// inputs. Like `std` and the run stack it's threaded: the server injects it into
/// each worker, whose nested `caos run` reads it back, so a whole run tree shares
/// one salt. Tests set it to a per-run random value, making their cache entries
/// collision-free across runs without ever touching Redis.
pub const SALT_ENV: &str = "CAOS_SALT";

/// Image-ref scheme marking an ordinary docker reference (vs. a git-image hash).
pub const DOCKER_SCHEME: &str = "docker://";

/// Marker entry naming a curry node: a CAS tree that pairs a `base` image ref
/// with an `args` subtree of bound arguments. `run`/`curry` expand it client-side
/// (merging the bound args under the call's args) so the server only ever
/// sees an ordinary image + args hash. The marker lets it be told apart from a
/// git-docker image tree, which it otherwise resembles. See [`unwrap_curry`].
pub const CURRY_MARKER: &str = ".caos-curry";

/// Directory under which objects are materialized. Override (e.g. for local
/// runs outside the container) with `CAOS_CAS_DIR`.
pub const CAS_DIR_ENV: &str = "CAOS_CAS_DIR";
pub const DEFAULT_CAS_DIR: &str = "/cas";

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
pub const MODE_FETCHED_DIR: u32 = 0o755;
const MODE_PLACEHOLDER_FILE: u32 = 0o400;
const MODE_PLACEHOLDER_DIR: u32 = 0o500;

/// Reserved suffix for the per-entry permission sidecars (see [`write_layer_metadata`]).
const META_SUFFIX: &str = ".caosmeta";

/// Disambiguates temp names created within a single process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Transport: how objects move between the client and the server's repo.
// ---------------------------------------------------------------------------

/// The store the client reads objects from and writes objects to. The two
/// binaries differ almost entirely in *this*: the worker speaks HTTP `/object`
/// to the server ([`HttpTransport`]); the CLI builds objects in its local working
/// repo and exchanges them with the server by negotiated git push/fetch.
///
/// `ensure_pushed`/`fetch_ref` are the network steps a *local-repo* transport
/// needs and an HTTP one doesn't, so they default to no-ops: the worker's
/// `put`/`get` already hit the server directly, while the CLI builds locally and
/// must explicitly push what it made and fetch what it wants.
pub trait Transport {
    /// Store a git object (`blob` or `tree`) and return its id.
    fn put_object(&self, kind: &str, content: &[u8]) -> Result<gix::ObjectId, String>;

    /// Fetch a git object's `(kind, content)` by hex hash.
    fn get_object(&self, hash: &str) -> Result<(String, Vec<u8>), String>;

    /// Ensure the server's repo holds the object graph reachable from `hash`.
    /// HTTP: a no-op — objects were already POSTed as they were built. Git: push
    /// it (under a content-addressed `refs/caos/req/<hash>`) so a subsequent
    /// compute can read it.
    fn ensure_pushed(&self, _hash: &str) -> Result<(), String> {
        Ok(())
    }

    /// Ingest the filesystem path named by a `:@=` arg `value`, returning its
    /// `(mode, oid)` — or `Ok(None)` if this transport doesn't read host paths.
    /// The default is `None`: the worker has no host filesystem (only `/cas`), so
    /// the caller handles it (an error for `run`/`curry`, a read-from-disk for
    /// `build-args` over `/object`). The git transport overrides this to ingest
    /// from the working repo, reusing git's recorded objects (see its impl).
    fn ingest_path(
        &self,
        _value: &str,
    ) -> Result<Option<(gix::objs::tree::EntryMode, gix::ObjectId)>, String> {
        Ok(None)
    }
}

/// Transport over the server's HTTP object API (`GET`/`POST /object`). Used by
/// the worker-side `caos`, where there's no local repo to negotiate against and
/// the server is a low-latency hop away on the docker network.
pub struct HttpTransport {
    base: String,
}

impl HttpTransport {
    /// Read the server URL from [`SERVER_ENV`].
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            base: server_url()?,
        })
    }
}

impl Transport for HttpTransport {
    fn put_object(&self, kind: &str, content: &[u8]) -> Result<gix::ObjectId, String> {
        let mut body = format!("{kind} {}\0", content.len()).into_bytes();
        body.extend_from_slice(content);

        let url = format!("{}/object/", self.base.trim_end_matches('/'));
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

    fn get_object(&self, hash: &str) -> Result<(String, Vec<u8>), String> {
        let url = format!("{}/object/{hash}", self.base.trim_end_matches('/'));
        let serialized = http_get(&url)?;
        let (kind, content) = parse_object(&serialized)?;
        Ok((kind.to_string(), content.to_vec()))
    }
}

/// The remote name a `caos-cli` working tree gives the server (`git remote add
/// caos <url>`). Push/fetch use it.
pub const CAOS_REMOTE: &str = "caos";

/// Transport over the server as a `caos` git remote, used by `caos-cli`. Objects
/// are built in the local working repo (cheap, in-process via gix) and exchanged
/// with the server by negotiated git push/fetch — so a large unchanged tree costs
/// almost nothing to "upload", and an edit ships only the changed blobs.
///
/// `put_object`/`get_object` are *local*: `put` writes a loose object,
/// `get` reads one (fetching from the remote first if it's missing, e.g. a
/// computation result). `ensure_pushed` is the one batch network step — it pushes
/// an object graph to the server so a `/run` can read it.
pub struct GitTransport {
    /// The discovered working repo, cached for local reads/writes.
    repo: gix::Repository,
    /// Its git directory, to re-open a fresh handle after a `git fetch` (the
    /// cached `repo`'s odb won't see a pack written behind its back).
    git_dir: PathBuf,
}

impl GitTransport {
    /// Discover the working repo from the current directory. `caos-cli` must run
    /// inside a git working tree that has the server as its `caos` remote.
    pub fn from_cwd() -> Result<Self, String> {
        let repo = gix::discover(".").map_err(|e| {
            format!("caos-cli must run inside a git working tree (none found): {e}")
        })?;
        let git_dir = repo.git_dir().to_path_buf();
        Ok(Self { repo, git_dir })
    }
}

impl Transport for GitTransport {
    fn put_object(&self, kind: &str, content: &[u8]) -> Result<gix::ObjectId, String> {
        match kind {
            "blob" => self
                .repo
                .write_blob(content)
                .map(|id| id.detach())
                .map_err(|e| format!("writing blob: {e}")),
            "tree" => {
                // Validate the canonical tree encoding, then write it as a real
                // tree object so its hash is a genuine git tree hash.
                let tree = gix::objs::TreeRef::from_bytes(content, self.repo.object_hash())
                    .map_err(|e| format!("invalid tree: {e}"))?;
                self.repo
                    .write_object(&tree)
                    .map(|id| id.detach())
                    .map_err(|e| format!("writing tree: {e}"))
            }
            other => Err(format!("cannot store object of kind {other}")),
        }
    }

    fn get_object(&self, hash: &str) -> Result<(String, Vec<u8>), String> {
        let oid = parse_oid(hash)?;
        if let Ok(object) = self.repo.find_object(oid) {
            return Ok((object.kind.to_string(), object.data.clone()));
        }
        // Missing locally — it's on the server (e.g. a computation result, which
        // lives there unreferenced). Fetch it by bare hash, then read it from a
        // fresh handle: the cached `repo` won't pick up the pack `git fetch` just
        // wrote.
        run_git(&["fetch", "--quiet", CAOS_REMOTE, hash])
            .map_err(|e| format!("fetching {hash} from {CAOS_REMOTE}: {e}"))?;
        let repo = gix::open(&self.git_dir)
            .map_err(|e| format!("reopening {}: {e}", self.git_dir.display()))?;
        let object = repo
            .find_object(oid)
            .map_err(|e| format!("object {hash} not found after fetch: {e}"))?;
        Ok((object.kind.to_string(), object.data.clone()))
    }

    fn ensure_pushed(&self, hash: &str) -> Result<(), String> {
        // Content-addressed ref: clobber-free across clients, idempotent (a
        // re-push of the same content is a no-op), and it persists as the
        // negotiation base for the next push, so an edited tree ships only its
        // delta. The push carries the whole object graph reachable from `hash`.
        let refspec = format!("{hash}:refs/caos/req/{hash}");
        run_git(&["push", "--quiet", CAOS_REMOTE, &refspec])
            .map_err(|e| format!("pushing {hash} to {CAOS_REMOTE}: {e}"))
    }

    fn ingest_path(
        &self,
        value: &str,
    ) -> Result<Option<(gix::objs::tree::EntryMode, gix::ObjectId)>, String> {
        let path = Path::new(value);
        // The value was declared a path (`:@=`), so a missing one is an error —
        // not silently a literal.
        if !path.exists() {
            return Err(format!("path not found: {value}"));
        }
        self.git_ingest(path).map(Some)
    }
}

impl GitTransport {
    /// Hash a filesystem path into the local repo, reusing git's recorded objects.
    /// A clean, tracked path keeps its committed hash with no read at all; a dirty
    /// or untracked one is hashed now — and for a directory only its *changed*
    /// files are re-read, the rest reusing their cached hash via a throwaway copy
    /// of the index (the same trick `git stash`/`commit` use). A path outside the
    /// worktree has no index to diff against, so it's read in full.
    fn git_ingest(
        &self,
        path: &Path,
    ) -> Result<(gix::objs::tree::EntryMode, gix::ObjectId), String> {
        use gix::objs::tree::EntryKind;
        let abs = path
            .canonicalize()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let rel = self
            .repo
            .workdir()
            .and_then(|w| abs.strip_prefix(w).ok())
            .map(Path::to_path_buf);

        if let Some(rel) = &rel {
            // Inside the worktree: reuse git's objects where we can.
            if self.is_clean(&abs)? {
                return self.tracked_entry(&abs); // committed hash, no read
            }
            if abs.is_dir() {
                return self.hash_dir(&abs, rel); // incremental: only changed files
            }
        }

        // A file (anywhere) hashes directly; a directory outside the worktree we
        // read in full (`store` walks it and writes objects via `put_object`).
        if abs.is_dir() {
            store(self, &PathBuf::from("/\0"), &abs)
        } else {
            let oid = self.hash_file(&abs)?;
            let exec = std::fs::metadata(&abs)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false);
            let kind = if exec {
                EntryKind::BlobExecutable
            } else {
                EntryKind::Blob
            };
            Ok((kind.into(), oid))
        }
    }

    /// Whether `abs` (inside the worktree) is clean and tracked — `git status`
    /// reports nothing for it (a dirty or untracked path is non-empty).
    fn is_clean(&self, abs: &Path) -> Result<bool, String> {
        let out = git_capture(
            &["status", "--porcelain", "--", &abs.to_string_lossy()],
            None,
        )?;
        Ok(out.trim().is_empty())
    }

    /// The `(mode, oid)` git records for a clean tracked path, read from `HEAD`
    /// (`ls-tree` prints `<mode> <type> <hash>\t<name>`). No file is read.
    fn tracked_entry(
        &self,
        abs: &Path,
    ) -> Result<(gix::objs::tree::EntryMode, gix::ObjectId), String> {
        let out = git_capture(&["ls-tree", "HEAD", "--", &abs.to_string_lossy()], None)?;
        let line = out
            .lines()
            .next()
            .ok_or_else(|| format!("{} not found in HEAD", abs.display()))?;
        let meta = line.split('\t').next().unwrap_or("");
        let mut fields = meta.split_whitespace();
        let mode = fields.next().unwrap_or("");
        let _kind = fields.next();
        let hash = fields.next().unwrap_or("");
        Ok((mode_from_git(mode)?, parse_oid(hash)?))
    }

    /// Hash a single file into the repo (`git hash-object -w`), returning its oid.
    fn hash_file(&self, abs: &Path) -> Result<gix::ObjectId, String> {
        let out = git_capture(&["hash-object", "-w", "--", &abs.to_string_lossy()], None)?;
        parse_oid(out.trim())
    }

    /// Hash a dirty/untracked directory `abs` (worktree-relative `rel`) into the
    /// repo, re-reading only its changed files. We copy the real index to a
    /// throwaway one (inheriting its stat-cache), `git add` the directory there,
    /// then `write-tree --prefix` to read back just that subtree.
    fn hash_dir(
        &self,
        abs: &Path,
        rel: &Path,
    ) -> Result<(gix::objs::tree::EntryMode, gix::ObjectId), String> {
        use gix::objs::tree::EntryKind;
        let tmp = temp_index_path()?;
        let real_index = self.git_dir.join("index");
        if real_index.exists() {
            std::fs::copy(&real_index, &tmp).map_err(|e| format!("copying index: {e}"))?;
        }
        let oid = (|| {
            git_capture(&["add", "--", &abs.to_string_lossy()], Some(&tmp))?;
            let prefix = format!("--prefix={}/", rel.to_string_lossy());
            let tree = git_capture(&["write-tree", &prefix], Some(&tmp))?;
            parse_oid(tree.trim())
        })();
        let _ = std::fs::remove_file(&tmp);
        Ok((EntryKind::Tree.into(), oid?))
    }
}

/// Run `git` (in the current working directory, i.e. the working repo) for the
/// network steps gix doesn't drive for us (push/fetch over smart-HTTP).
fn run_git(args: &[&str]) -> Result<(), String> {
    git_capture(args, None).map(|_| ())
}

/// Run `git` in the working repo and return its stdout; error on failure. With
/// `index` set, `GIT_INDEX_FILE` points at a throwaway index (so `git add` /
/// `write-tree` don't touch the real one). Used for both the network steps and
/// the path-ingestion plumbing.
fn git_capture(args: &[&str], index: Option<&Path>) -> Result<String, String> {
    let mut command = std::process::Command::new("git");
    command.args(args);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command
        .output()
        .map_err(|e| format!("running git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Map a git tree-entry mode string (as `ls-tree` prints it) to a gix `EntryMode`.
fn mode_from_git(mode: &str) -> Result<gix::objs::tree::EntryMode, String> {
    use gix::objs::tree::EntryKind;
    let kind = match mode {
        "40000" | "040000" => EntryKind::Tree,
        "100644" => EntryKind::Blob,
        "100755" => EntryKind::BlobExecutable,
        "120000" => EntryKind::Link,
        "160000" => EntryKind::Commit,
        other => return Err(format!("unknown git mode {other:?}")),
    };
    Ok(kind.into())
}

/// A fresh, unique throwaway-index path (under the system temp dir).
fn temp_index_path() -> Result<PathBuf, String> {
    let base = std::env::temp_dir().join("caos-index");
    std::fs::create_dir_all(&base).map_err(|e| format!("creating {}: {e}", base.display()))?;
    let pid = std::process::id();
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(base.join(format!("{pid}.{seq}")))
}

/// Base URL of the caos server (storage + compute), from [`SERVER_ENV`].
pub fn server_url() -> Result<String, String> {
    std::env::var(SERVER_ENV)
        .map_err(|_| format!("{SERVER_ENV} must be set to the caos server URL"))
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

// ---------------------------------------------------------------------------
// Object model helpers.
// ---------------------------------------------------------------------------

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

/// Parse a hex git hash (tolerating surrounding whitespace).
fn parse_oid(hex: &str) -> Result<gix::ObjectId, String> {
    gix::ObjectId::from_hex(hex.trim().as_bytes()).map_err(|e| format!("invalid hash {hex:?}: {e}"))
}

/// A bare 40-char SHA-1 hash, naming a git object directly (a git image or a
/// curry node). Length-checked so a short CAS-relative path isn't mistaken for
/// one.
fn is_hex_hash(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Store `content` as a `kind` object via the transport and return its hash.
fn post_object(t: &dyn Transport, kind: &str, content: &[u8]) -> Result<gix::ObjectId, String> {
    t.put_object(kind, content)
}

/// Encode `entries` as a git tree object and store it via the transport,
/// returning its hash. Shared by `store` (real directories) and the args-tree
/// builders (the synthesized trees).
fn post_tree(
    t: &dyn Transport,
    mut entries: Vec<gix::objs::tree::Entry>,
) -> Result<gix::ObjectId, String> {
    // Git requires tree entries in a specific order; Entry's Ord implements it.
    entries.sort();
    let mut buf = Vec::new();
    gix::objs::Tree { entries }
        .write_to(&mut buf)
        .map_err(|e| format!("encoding tree: {e}"))?;
    t.put_object("tree", &buf)
}

/// Fetch object `hash` and write it to `target` (blob → file, tree → directory).
pub fn fetch_and_materialize(t: &dyn Transport, target: &Path, hash: &str) -> Result<(), String> {
    let (kind, content) = t.get_object(hash)?;

    // The transport returns the object's true type, so no guessing.
    if kind == "tree" {
        let tree = gix::objs::TreeRef::from_bytes(&content, gix::hash::Kind::Sha1)
            .map_err(|e| format!("malformed tree {hash}: {e}"))?;
        write_tree(target, hash, &tree)
    } else {
        write_file(target, hash, &content)
    }
}

/// Fetch object `hash`; if it's a tree, return its entries as owned values, else
/// `None`.
fn fetch_tree_entries(
    t: &dyn Transport,
    hash: &str,
) -> Result<Option<Vec<gix::objs::tree::Entry>>, String> {
    let (kind, content) = t.get_object(hash)?;
    if kind != "tree" {
        return Ok(None);
    }
    let tree = gix::objs::TreeRef::from_bytes(&content, gix::hash::Kind::Sha1)
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
fn fetch_blob_string(t: &dyn Transport, hash: &str) -> Result<String, String> {
    let (kind, content) = t.get_object(hash)?;
    if kind != "blob" {
        return Err(format!("expected a blob at {hash}, got {kind}"));
    }
    let text = std::str::from_utf8(&content).map_err(|e| format!("blob {hash} not UTF-8: {e}"))?;
    Ok(text.trim().to_string())
}

// ---------------------------------------------------------------------------
// CAS materialization (filesystem side; transport-independent except fetches).
// ---------------------------------------------------------------------------

/// CAS root directory (`/cas`, or `$CAOS_CAS_DIR`).
pub fn cas_dir() -> PathBuf {
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
pub fn read_hash(path: &Path) -> Result<String, String> {
    let bytes = xattr::get(path, HASH_XATTR)
        .map_err(|e| format!("reading {HASH_XATTR} from {}: {e}", path.display()))?
        .ok_or_else(|| format!("no {HASH_XATTR} recorded for {}", path.display()))?;
    String::from_utf8(bytes).map_err(|e| format!("invalid {HASH_XATTR} on {}: {e}", path.display()))
}

/// Fail fast if the CAS directory can't store the `user.*` xattrs we use to
/// record source hashes (some filesystems — tmpfs on older kernels, certain
/// overlay setups — don't support them).
pub fn probe_xattr(cas: &Path) -> Result<(), String> {
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

/// Whether `path` has already been fetched, as opposed to an unexpanded
/// placeholder. Loaded content is group/other-readable; a placeholder is
/// owner-only (see `MODE_FETCHED_*` vs `MODE_PLACEHOLDER_*`), so the read bits
/// double as the "is this loaded yet?" marker.
fn is_loaded(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.permissions().mode() & 0o044 != 0)
        .unwrap_or(false)
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

/// Record a result as a typed, tagged placeholder at `target`, fetching nothing:
/// an empty directory for a tree, an empty file for a blob, tagged with `hash` and
/// owner-only (the placeholder mode). It's an unloaded handle — `caos put`
/// references it by its recorded hash (no content needed) and `caos get` expands
/// it on demand — so a `caos run` result never has to come back to the caller.
fn write_placeholder(target: &Path, kind: &str, hash: &str) -> Result<(), String> {
    atomically(target, |tmp| {
        let mode = match kind {
            "tree" => {
                std::fs::create_dir(tmp).map_err(|e| format!("creating {}: {e}", tmp.display()))?;
                MODE_PLACEHOLDER_DIR
            }
            "blob" => {
                std::fs::File::create(tmp)
                    .map_err(|e| format!("creating {}: {e}", tmp.display()))?;
                MODE_PLACEHOLDER_FILE
            }
            other => return Err(format!("unknown result type {other:?}")),
        };
        set_hash(tmp, hash.as_bytes())?;
        set_mode(tmp, mode)
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
pub fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("setting mode on {}: {e}", path.display()))
}

/// Parse `key` from the environment as a `u32`, or `None` if unset/unparseable.
pub fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// Materialize the placeholder at `target` from its recorded hash, then — if it
/// became a directory and `depth` allows another level — expand each child the
/// same way. `depth` is the number of levels left to load: `Some(1)` stops after
/// `target` (a plain `get`), `Some(n)` descends `n - 1` more levels, and `None`
/// loads the whole subtree. (A git object graph is a finite DAG, so unbounded
/// recursion always terminates at the blobs.)
fn expand(t: &dyn Transport, target: &Path, depth: Option<u32>) -> Result<(), String> {
    // Fetch only an unexpanded placeholder. An already-loaded node is left as is
    // and we just descend into it, so `get -r` is idempotent and can finish
    // loading a tree that was already partially expanded (e.g. after `get-hash`).
    // Re-fetching here would also fail anyway: renaming the fresh copy over a
    // non-empty directory is `ENOTEMPTY`.
    if !is_loaded(target) {
        let hash = read_hash(target)?;
        fetch_and_materialize(t, target, &hash)?;
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
            expand(t, &child, child_depth)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands.
// ---------------------------------------------------------------------------

/// `get-hash <hash> <path>` — fetch `<hash>` and materialize it at `<path>`,
/// which must be a direct child of the CAS directory.
pub fn get_hash(t: &dyn Transport, hash: &str, path: &str) -> Result<(), String> {
    let cas = cas_dir();
    let target = validate_target(&cas, path)?;
    probe_xattr(&cas)?;
    fetch_and_materialize(t, &target, hash)
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
pub fn get(t: &dyn Transport, path: &str, depth: Option<u32>) -> Result<(), String> {
    let cas = cas_dir();
    let target = validate_descendant(&cas, path)?;
    probe_xattr(&cas)?;
    expand(t, &target, depth)
}

/// Parse `get`'s arguments: an optional recursion flag plus exactly one path.
/// `-r` and bare `--recursive` mean the whole subtree (`None`); `--recursive=<n>`
/// means `n` levels (`n >= 1`); absent, the default is one level (`Some(1)`).
pub fn parse_get(args: &[String]) -> Result<(&str, Option<u32>), String> {
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

/// `put <src-path> <cas-path>` — recursively store `<src-path>` (a path outside
/// the CAS) into the server and record the result at `<cas-path>`, a
/// direct child of the CAS directory.
///
/// Files are stored as blobs and directories as trees — both as real git objects
/// (their hashes are genuine git tree/blob hashes). A symlink that resolves to
/// something already in the CAS is *not* re-read — its recorded hash is reused,
/// so shared content is stored once.
pub fn put(t: &dyn Transport, src: &str, dst: &str) -> Result<(), String> {
    let cas = cas_dir();
    let target = validate_target(&cas, dst)?;
    probe_xattr(&cas)?;
    let cas_real = cas
        .canonicalize()
        .map_err(|e| format!("CAS directory {}: {e}", cas.display()))?;

    let (_, oid) = store(t, &cas_real, Path::new(src))?;
    fetch_and_materialize(t, &target, &oid.to_string())
}

/// Recursively store `path` via the transport, returning the git tree entry
/// (mode + oid) that refers to it.
fn store(
    t: &dyn Transport,
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
        let oid = post_object(t, "blob", link.as_os_str().as_bytes())?;
        return Ok((EntryKind::Link.into(), oid));
    }

    if ft.is_dir() {
        let mut entries = Vec::new();
        for dirent in std::fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))? {
            let dirent = dirent.map_err(|e| format!("{}: {e}", path.display()))?;
            let (mode, oid) = store(t, cas_real, &dirent.path())?;
            entries.push(gix::objs::tree::Entry {
                mode,
                filename: dirent.file_name().into_vec().into(),
                oid,
            });
        }
        let oid = post_tree(t, entries)?;
        return Ok((EntryKind::Tree.into(), oid));
    }

    if ft.is_file() {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let oid = post_object(t, "blob", &data)?;
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
/// directory. Shared by `store` (symlinks into the CAS) and `build_arg_entries`
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

/// `import-image <docker-archive> <cas-path>` — store a docker-archive image (the
/// kind `nix build .#caos-*-docker` / `docker save` produce) into the CAS in
/// git-docker form: a tree holding `config.json` (the image config, verbatim) and
/// one `layer<NN>` subtree per layer (the layer tar's extracted filesystem),
/// materialized at `<cas-path>`. `run <cas-path>` then has the server convert it
/// back into a real image. Prints the stored git-docker tree's hash.
///
/// Only the layer *contents* are captured (files, the exec bit, and symlinks);
/// mtimes/owners are dropped, which is fine — the server re-tars the trees
/// deterministically and generates the diff_ids itself.
pub fn import_image(t: &dyn Transport, archive: &str, dst: &str) -> Result<(), String> {
    use gix::objs::tree::{Entry, EntryKind};

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
            oid: post_object(t, "blob", &config_bytes)?,
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
            let (_, oid) = store(t, &cas_real, &layer_dir)?;
            entries.push(Entry {
                mode: EntryKind::Tree.into(),
                filename: format!("layer{i:02}").into_bytes().into(),
                oid,
            });
            eprintln!("imported layer{i:02} from {layer_path}");
        }

        let image_oid = post_tree(t, entries)?;
        fetch_and_materialize(t, &target, &image_oid.to_string())?;
        // Print the stored git-docker tree's hash, e.g. so a script can assemble
        // it into a larger tree (the built-ins library does this).
        println!("{image_oid}");
        Ok(())
    })();

    let _ = std::fs::remove_dir_all(&work);
    outcome
}

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

/// `build-args [--name=value ...]` — assemble an args git tree and print its
/// hash. For each pair, a `value` that names an existing path (relative to the
/// working directory) is stored recursively from the filesystem, so the entry
/// references that content's own hash (git's hash for the path); any other value
/// is stored verbatim as a blob. The printed hash is meant to be passed to
/// `caos entrypoint --args=<hash>` (which is what `run-worker-bash.sh` does).
pub fn build_args(t: &dyn Transport, kvs: &[String]) -> Result<(), String> {
    let hash = build_host_args_tree(t, kvs)?;
    println!("{hash}");
    Ok(())
}

/// The args-tree builder behind `build-args`: like [`build_arg_entries`], but a
/// `value` naming an existing filesystem path is ingested as its content rather
/// than reused from the CAS. Ingestion goes through [`Transport::ingest_path`],
/// so the git transport (`caos-cli`) reuses git's recorded objects (see
/// [`GitTransport::ingest_path`]) while the HTTP transport (a worker) reads it
/// from disk via [`store`]. The tree is never written to the filesystem.
fn build_host_args_tree(t: &dyn Transport, kvs: &[String]) -> Result<gix::ObjectId, String> {
    use gix::objs::tree::{Entry, EntryKind};

    // There's no CAS here, so `store` should never treat a symlink as one
    // pointing into a CAS. A NUL byte can't appear in a real (canonicalized)
    // path, so no path ever starts with this sentinel — the CAS-reuse branch
    // stays dormant and symlinks are stored as plain git symlinks.
    let no_cas = PathBuf::from("/\0");

    let mut entries = Vec::new();
    for kv in kvs {
        let (name, value) = parse_kv(kv)?;

        let (mode, oid) = match value {
            // `--name=value` — the literal verbatim as a blob.
            ArgValue::Literal(v) => (
                EntryKind::Blob.into(),
                post_object(t, "blob", v.as_bytes())?,
            ),
            // `--name:@=path` — the git transport ingests it (reusing git's
            // objects); the HTTP transport (a worker, e.g. run-worker-bash's)
            // reads it from disk and uploads it.
            ArgValue::Path(p) => match t.ingest_path(p)? {
                Some(entry) => entry,
                None => store(t, &no_cas, Path::new(p))?,
            },
        };

        entries.push(Entry {
            mode,
            filename: name.as_bytes().to_vec().into(),
            oid,
        });
    }

    post_tree(t, entries)
}

/// The per-arg tree entries that make up an args tree — `run`/`curry` merge call
/// args with a curry node's bound args, then `post_tree` the result.
///
/// Each `--name[:type]=value` becomes a tree entry `name` (see [`parse_kv`]):
/// * `--name=value` — a literal, stored verbatim as a blob;
/// * `--name:@=path` inside the CAS — references the object that path was
///   materialized from (its recorded hash);
/// * `--name:@=path` elsewhere — a host path, ingested via the transport (the git
///   transport reuses git's recorded objects — see [`GitTransport::ingest_path`]);
///   a worker has no host filesystem, so this is an error there.
fn build_arg_entries(
    t: &dyn Transport,
    cas: &Path,
    kvs: &[String],
) -> Result<Vec<gix::objs::tree::Entry>, String> {
    use gix::objs::tree::{Entry, EntryKind};

    // Canonical CAS root, resolved lazily — only needed if a CAS path appears.
    let cas_real = cas.canonicalize();

    let mut entries = Vec::new();
    for kv in kvs {
        let (name, value) = parse_kv(kv)?;

        let (mode, oid) = match value {
            // `--name=value` — store the literal verbatim as a blob.
            ArgValue::Literal(v) => (
                EntryKind::Blob.into(),
                post_object(t, "blob", v.as_bytes())?,
            ),
            // `--name:@=path` under the CAS — reference whatever it was made from.
            ArgValue::Path(p) if Path::new(p).starts_with(cas) => {
                let canon = Path::new(p)
                    .canonicalize()
                    .map_err(|e| format!("{p}: {e}"))?;
                let cas_real = cas_real
                    .as_ref()
                    .map_err(|e| format!("CAS directory {}: {e}", cas.display()))?;
                if !canon.starts_with(cas_real) {
                    return Err(format!("{p} resolves outside {}", cas.display()));
                }
                cas_entry(&canon)?
            }
            // `--name:@=path` elsewhere — ingest a host path (git transport only;
            // the worker has no host filesystem, so it errors clearly).
            ArgValue::Path(p) => t.ingest_path(p)?.ok_or_else(|| {
                format!("`{name}`: {p:?} is a host path, but this client only reads /cas paths")
            })?,
        };

        entries.push(Entry {
            mode,
            filename: name.as_bytes().to_vec().into(),
            oid,
        });
    }

    Ok(entries)
}

/// A parsed `--name[:type]=value` argument value. The type marker lives in the
/// operator, not the value, so the value is unconstrained (it may start with
/// anything, no escaping). Bare `=` is a literal; `:@=` marks a path. The grammar
/// is extensible — a new type adds a variant here and a case in [`parse_kv`].
enum ArgValue<'a> {
    /// `--name=value` — the value verbatim, stored as a blob.
    Literal(&'a str),
    /// `--name:@=path` — the value names a filesystem path to resolve/ingest.
    Path(&'a str),
}

/// Split a `--name[:type]=value` argument into its name and typed value,
/// validating that the name is a single path component (it becomes a tree-entry
/// filename). The only type is `@` (a path); bare is a literal.
fn parse_kv(kv: &str) -> Result<(&str, ArgValue<'_>), String> {
    let body = kv
        .strip_prefix("--")
        .ok_or_else(|| format!("argument must look like --name=value, got: {kv}"))?;
    let (key, value) = body
        .split_once('=')
        .ok_or_else(|| format!("argument must look like --name[:type]=value, got: {kv}"))?;
    // The key is `name` (literal) or `name:type` (typed); the type sits before `=`.
    let (name, value) = match key.split_once(':') {
        None => (key, ArgValue::Literal(value)),
        Some((name, "@")) => (name, ArgValue::Path(value)),
        Some((_, ty)) => {
            return Err(format!(
                "unknown argument type {ty:?} in {kv:?}; use --name=value (literal) \
                 or --name:@=value (path)"
            ))
        }
    };
    if name.is_empty() || name.contains('/') {
        return Err(format!(
            "argument name must be a single path component, got: {name:?}"
        ));
    }
    Ok((name, value))
}

/// `run <image> <output> -- [--name=value ...]` — assemble the args into a git
/// tree, ask the server to run `<image>` over that tree, and materialize the
/// result at `<output>` (a direct child of the CAS directory).
pub fn caos_run(
    t: &dyn Transport,
    image: &str,
    output: &str,
    kvs: &[String],
) -> Result<(), String> {
    let cas = cas_dir();
    let target = validate_target(&cas, output)?;
    probe_xattr(&cas)?;

    // Resolve the image: a CAS path becomes the git hash recorded on it, so the
    // server converts it from our git-docker form; a `docker://` ref or a
    // bare hash is sent through unchanged.
    let image = resolve_run_image(&cas, image)?;

    // Expand any curry layers: pull the underlying image out and collect the args
    // bound into it, so the server only ever sees a plain image + args.
    let (image, bound) = unwrap_curry(t, &image)?;

    // Build the call's args, then merge them over the bound ones (call wins).
    // Nothing is written under /cas — the worker materializes the tree itself.
    let call = build_arg_entries(t, &cas, kvs)?;
    let args_tree = post_tree(t, merge_entries(bound, call))?;

    // The built-in tree (`std`): inherited from CAOS_STD inside a worker, or
    // resolved from the `refs/caos/std` ref at the top. Part of the request so the
    // server keys on it and threads it down (materialized at /cas/std).
    let std = run_std()?;
    // The cache-busting salt (empty by default), threaded like std.
    let salt = run_salt();

    // Bundle the request as a content-addressed object {image, args, std, salt};
    // its hash is the request id (and the server's cache key). Get it onto the
    // server — a no-op POST-as-you-go for the HTTP transport, a push for the git
    // one — plus a git image's own objects (referenced by hash in a blob, so not
    // carried by the request tree).
    let req = build_request(t, &image, &args_tree, &std, &salt)?;
    t.ensure_pushed(&req.to_string())?;
    if is_hex_hash(&image) {
        t.ensure_pushed(&image)?;
    }

    // Trigger compute; the server runs the container and returns the result's
    // "<type> <hash>" (and, for a top-level run, pins refs/caos/res/<req> at it).
    let (kind, result) = request_compute(&req.to_string())?;

    // Record the result as a typed, tagged placeholder — fetch nothing. The result
    // stays on the server; `caos get <output>` loads it on demand if wanted.
    write_placeholder(&target, &kind, &result)
}

/// Bundle a run request as a content-addressed object: a tree `{image, args,
/// std, salt}` — `image`/`std`/`salt` as blobs, `args` as the args subtree. Its
/// hash is the request id: the server's cache key and the result-ref rendezvous.
/// (The git-docker image's own objects aren't reachable from here — `image` is a
/// blob naming it by hash — so `caos run` pushes them separately.)
fn build_request(
    t: &dyn Transport,
    image: &str,
    args_tree: &gix::ObjectId,
    std: &str,
    salt: &str,
) -> Result<gix::ObjectId, String> {
    use gix::objs::tree::{Entry, EntryKind};
    let entries = vec![
        Entry {
            mode: EntryKind::Blob.into(),
            filename: b"image".to_vec().into(),
            oid: post_object(t, "blob", image.as_bytes())?,
        },
        Entry {
            mode: EntryKind::Tree.into(),
            filename: b"args".to_vec().into(),
            oid: *args_tree,
        },
        Entry {
            mode: EntryKind::Blob.into(),
            filename: b"std".to_vec().into(),
            oid: post_object(t, "blob", std.as_bytes())?,
        },
        Entry {
            mode: EntryKind::Blob.into(),
            filename: b"salt".to_vec().into(),
            oid: post_object(t, "blob", salt.as_bytes())?,
        },
    ];
    post_tree(t, entries)
}

/// The built-in tree hash (`std`) for a run. Inside a worker the server sets
/// [`STD_ENV`], so reuse it (threading). At the top, resolve the built-ins ref
/// ([`STD_REF_ENV`], default `refs/caos/std`) from the local repo; tolerate its
/// absence (no built-ins published) — a worker that needs them will fail clearly.
fn run_std() -> Result<String, String> {
    if let Ok(std) = std::env::var(STD_ENV) {
        return Ok(std);
    }
    let refname = std::env::var(STD_REF_ENV).unwrap_or_else(|_| DEFAULT_STD_REF.to_string());
    Ok(resolve_ref(&refname).unwrap_or_default())
}

/// The cache-busting salt for this run (see [`SALT_ENV`]): read from `CAOS_SALT`,
/// empty if unset. Threaded — the server injects it into each worker, whose
/// nested `caos run` reads it back here — so a whole run tree shares one salt.
fn run_salt() -> String {
    std::env::var(SALT_ENV).unwrap_or_default()
}

/// Resolve a git ref (e.g. `refs/caos/std`) to its tree hash, read from the local
/// repository. Peels tags and commits to a tree. No server round-trip: the CLI
/// already has the refs (it fetched them from the `caos` remote).
pub fn resolve_ref(name: &str) -> Result<String, String> {
    let repo = gix::discover(".").map_err(|e| format!("no git repo for ref {name}: {e}"))?;
    let mut reference = repo
        .find_reference(name)
        .map_err(|e| format!("ref {name} not found: {e}"))?;
    let id = reference
        .peel_to_id()
        .map_err(|e| format!("peeling ref {name}: {e}"))?;
    let object = id.object().map_err(|e| format!("reading {id}: {e}"))?;
    let tree = match object.kind {
        gix::object::Kind::Tree => id.detach(),
        gix::object::Kind::Commit => object
            .try_into_commit()
            .map_err(|e| format!("{name}: {e}"))?
            .tree_id()
            .map_err(|e| format!("{name} has no tree: {e}"))?
            .detach(),
        other => {
            return Err(format!(
                "ref {name} points at a {other}, not a tree or commit"
            ))
        }
    };
    Ok(tree.to_string())
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
pub fn caos_curry(t: &dyn Transport, image: &str, kvs: &[String]) -> Result<(), String> {
    use gix::objs::tree::{Entry, EntryKind};

    let cas = cas_dir();

    let image = resolve_run_image(&cas, image)?;
    let (image, bound) = unwrap_curry(t, &image)?;

    // New bindings override any already bound to the same name.
    let args = merge_entries(bound, build_arg_entries(t, &cas, kvs)?);
    let args_tree = post_tree(t, args)?;

    let entries = vec![
        Entry {
            mode: EntryKind::Blob.into(),
            filename: b"base".to_vec().into(),
            oid: post_object(t, "blob", image.as_bytes())?,
        },
        Entry {
            mode: EntryKind::Tree.into(),
            filename: b"args".to_vec().into(),
            oid: args_tree,
        },
        Entry {
            mode: EntryKind::Blob.into(),
            filename: CURRY_MARKER.as_bytes().to_vec().into(),
            oid: post_object(t, "blob", b"1")?,
        },
    ];
    println!("{}", post_tree(t, entries)?);
    Ok(())
}

/// Peel any curry layers off `image` (a resolved ref: `docker://…` or a git
/// hash), returning the underlying plain image and the args bound into it. A
/// caller merges these *under* its own args, so call-time args win; with curry's
/// flattening there is normally a single layer, but nested layers are handled
/// defensively (an outer binding wins over an inner one for the same name).
fn unwrap_curry(
    t: &dyn Transport,
    image: &str,
) -> Result<(String, Vec<gix::objs::tree::Entry>), String> {
    let mut image = image.to_string();
    let mut bound = Vec::new();
    while is_hex_hash(&image) {
        match curry_node(t, &image)? {
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
    t: &dyn Transport,
    hash: &str,
) -> Result<Option<(String, Vec<gix::objs::tree::Entry>)>, String> {
    let entries = match fetch_tree_entries(t, hash)? {
        Some(entries) => entries,
        None => return Ok(None),
    };
    if !entries
        .iter()
        .any(|e| entry_name(e) == CURRY_MARKER.as_bytes())
    {
        return Ok(None);
    }
    let oid_of = |name: &[u8]| {
        entries
            .iter()
            .find(|e| entry_name(e) == name)
            .map(|e| e.oid)
            .ok_or_else(|| {
                format!(
                    "curry node {hash} missing {:?}",
                    String::from_utf8_lossy(name)
                )
            })
    };
    let base_ref = fetch_blob_string(t, &oid_of(b"base")?.to_string())?;
    let args = fetch_tree_entries(t, &oid_of(b"args")?.to_string())?
        .ok_or_else(|| format!("curry node {hash} 'args' is not a tree"))?;
    Ok(Some((base_ref, args)))
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

/// Trigger compute for request `req` and return the result's `(type, hash)`. The
/// server runs the container and replies `"<type> <hash>"`. `&stack=` rides along
/// for cycle detection — it's threaded state, not part of the request's identity.
fn request_compute(req: &str) -> Result<(String, String), String> {
    let base = server_url()?;
    let mut url = format!("{}/run?req={}", base.trim_end_matches('/'), req);
    if let Ok(stack) = std::env::var(RUN_STACK_ENV) {
        if !stack.is_empty() {
            url.push_str("&stack=");
            url.push_str(&percent_encode(&stack));
        }
    }
    let body = http_get(&url)?;
    let text =
        String::from_utf8(body).map_err(|e| format!("server returned invalid UTF-8: {e}"))?;
    let (kind, hash) = text
        .trim()
        .split_once(' ')
        .ok_or_else(|| format!("server returned a malformed result: {:?}", text.trim()))?;
    if hash.is_empty() {
        return Err("server returned an empty result".to_string());
    }
    Ok((kind.to_string(), hash.to_string()))
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

/// Program name from `argv[0]` (`caos`/`caos-cli` in the image or build tree),
/// for diagnostics and usage.
pub fn prog_name(args: &[String]) -> &str {
    args.first()
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .unwrap_or("caos")
}
