//! caos client library: the logic shared by the two `caos` binaries.
//!
//! There are two clients (see the crate's `bin/`):
//!
//! * **`caos`** — the worker-side client baked setuid-root into worker images.
//!   It talks to the server over HTTP (`/object`) and runs the container
//!   `runner` (jobs arrive by long-poll; see `design/runner-protocol.md`). It
//!   never triggers compute — its `map-then` records a map-then continuation
//!   the server resolves after the worker's job finishes.
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
use std::io::{IsTerminal, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gix::objs::WriteTo;

mod chat;
pub use chat::cli_chat;

/// Base URL of the caos server (storage + compute), e.g. `http://caos-server`.
pub const SERVER_ENV: &str = "CAOS_SERVER_URL";

/// The built-in tree hash (`std`) in effect for this run. The server sets it on
/// each worker it spawns (materialized at `/cas/std`) and threads it into every
/// promise sub-run, so it rides down the whole tree. At the top it's unset, and
/// the ref named by [`STD_REF_ENV`] is resolved instead. `std` *is* part of the
/// result cache key (it names the standard library a worker can reach).
pub const STD_ENV: &str = "CAOS_STD";
/// Ref resolved to `std` at the top of a run (overridable). Default
/// `refs/caos/std`, read from the local repo.
pub const STD_REF_ENV: &str = "CAOS_STD_REF";
pub const DEFAULT_STD_REF: &str = "refs/caos/std";

/// An opaque cache-busting value mixed into every run's request — and so into its
/// `reqHash` and cache key. Empty by default, so runs are cached purely by their
/// inputs. Like `std` it's threaded: the server injects it into each worker and
/// into every promise sub-run, so a whole run tree shares one salt. Tests set it
/// to a per-run random value, making their cache entries collision-free across
/// runs without ever touching Redis.
pub const SALT_ENV: &str = "CAOS_SALT";

/// Image-ref scheme marking an ordinary docker reference (vs. a git-image hash).
pub const DOCKER_SCHEME: &str = "docker://";

/// Marker entry naming a curry node: a CAS tree that pairs a `base` image ref
/// with an `args` subtree of bound arguments. `run`/`curry` expand it client-side
/// (merging the bound args under the call's args, then folding the base in as the
/// args' `image` entry) so the server only ever sees an ordinary args tree. The
/// marker lets it be told apart from a
/// git-docker image tree, which it otherwise resembles. See [`unwrap_curry`].
pub const CURRY_MARKER: &str = ".caos-curry";

/// Directory under which objects are materialized. Override (e.g. for local
/// runs outside the container) with `CAOS_CAS_DIR`.
pub const CAS_DIR_ENV: &str = "CAOS_CAS_DIR";
pub const DEFAULT_CAS_DIR: &str = "/cas";

/// xattr recording the git hash a materialized path came from.
const HASH_XATTR: &str = "user.caos.hash";
/// xattr recording a path's object *kind* when it isn't implied by the node's
/// shape: `promise` (a `caos map-then`/`run-then` continuation, recorded as a
/// file placeholder) or `commit` (a commit-valued path — as a placeholder, and
/// still after fetching, since a materialized commit is a file holding the raw
/// commit object). Absent otherwise: a directory is a tree, a file a blob.
const KIND_XATTR: &str = "user.caos.kind";
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
    /// a non-CAS path there is an error. The git transport overrides this to
    /// ingest from the working repo, reusing git's recorded objects (see its impl).
    fn ingest_path(
        &self,
        _value: &str,
    ) -> Result<Option<(gix::objs::tree::EntryMode, gix::ObjectId)>, String> {
        Ok(None)
    }

    /// Resolve a revspec (e.g. `HEAD`, a branch name) named by a `:commit=` arg
    /// to a commit id — or `Ok(None)` if this transport has no repo to resolve
    /// against. The default is `None`: the worker has no working repo (a commit
    /// reaches it as a hash or a `/cas` path); the git transport overrides this
    /// to resolve against the working repo.
    fn resolve_revspec(&self, _rev: &str) -> Result<Option<gix::ObjectId>, String> {
        Ok(None)
    }

    /// Base URL of the caos server for compute (`/run`). HTTP transport: the
    /// configured server (the worker's injected [`SERVER_ENV`]). Git transport:
    /// the `caos` remote's URL — the same place the CLI already points, so a
    /// person never sets [`SERVER_ENV`] themselves.
    fn server_url(&self) -> Result<String, String>;
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

    fn server_url(&self) -> Result<String, String> {
        Ok(self.base.clone())
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
            "commit" => {
                // Validate the commit encoding, then store the raw bytes (not a
                // re-encoding), so the hash matches the bytes exactly — the same
                // rule the server's `post_object` applies.
                gix::objs::CommitRef::from_bytes(content, self.repo.object_hash())
                    .map_err(|e| format!("invalid commit: {e}"))?;
                gix::objs::Write::write_buf(&self.repo.objects, gix::object::Kind::Commit, content)
                    .map_err(|e| format!("writing commit: {e}"))
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
        fetch_object(hash)?;
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

    fn resolve_revspec(&self, rev: &str) -> Result<Option<gix::ObjectId>, String> {
        // `^{commit}` peels annotated tags but *requires* a commit at the end —
        // a revspec naming a tree/blob is an error, never silently accepted.
        let out = git_capture(
            &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
            None,
        )
        .map_err(|e| format!("resolving {rev:?} to a commit: {e}"))?;
        parse_oid(out.trim()).map(Some)
    }

    fn server_url(&self) -> Result<String, String> {
        // The `caos` remote's URL *is* the server: the CLI already pushes/fetches
        // objects there, and /run lives at the same host. So a person configures
        // the server once (`git remote add caos <url>`) and never sets an env var.
        let remote = self.repo.find_remote(CAOS_REMOTE).map_err(|e| {
            format!(
                "no `{CAOS_REMOTE}` git remote (add it with \
                 `git remote add {CAOS_REMOTE} <server-url>`): {e}"
            )
        })?;
        let url = remote
            .url(gix::remote::Direction::Fetch)
            .ok_or_else(|| format!("`{CAOS_REMOTE}` remote has no fetch URL"))?;
        Ok(url.to_bstring().to_string())
    }
}

impl GitTransport {
    /// Hash a filesystem path into the local repo, reusing git's recorded objects.
    /// Only git-tracked paths inside the worktree can be ingested (the nix-flakes
    /// rule: a build sees only what git knows about). A clean, tracked path keeps
    /// its committed hash with no read at all; a tracked path with uncommitted
    /// edits is hashed now from the working tree — and for a directory only its
    /// *changed* tracked files are re-read, the rest reusing their cached hash via
    /// a throwaway copy of the index (the same trick `git stash`/`commit` use),
    /// while untracked files inside it are excluded. A path outside the worktree,
    /// or one git doesn't track, is an error.
    fn git_ingest(
        &self,
        path: &Path,
    ) -> Result<(gix::objs::tree::EntryMode, gix::ObjectId), String> {
        use gix::objs::tree::EntryKind;
        let abs = path
            .canonicalize()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // Canonicalize the worktree root too before comparing: `gix::discover(".")`
        // records a cwd-relative, symlink-unresolved workdir, whereas `abs` is
        // fully resolved — so a raw `strip_prefix` would miss a path that really is
        // inside the tree.
        let workdir = self
            .repo
            .workdir()
            .map(|w| w.canonicalize().unwrap_or_else(|_| w.to_path_buf()));
        let rel = workdir
            .as_deref()
            .and_then(|w| abs.strip_prefix(w).ok())
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                format!(
                    "{}: outside the git worktree; caos only ingests git-tracked paths",
                    path.display()
                )
            })?;

        // Inside the worktree: reuse git's objects where we can.
        if self.is_clean(&abs)? {
            return self.tracked_entry(&abs, &rel); // committed hash, no read
        }
        // Dirty or untracked. Refuse anything git doesn't track — untracked files
        // are invisible to a build, just as they are to a nix flake.
        if !self.is_tracked(&abs)? {
            return Err(format!(
                "{}: not tracked by git; caos only ingests git-tracked paths \
                 (add it with `git add`)",
                path.display()
            ));
        }
        if abs.is_dir() {
            return self.hash_dir(&abs, &rel); // incremental: only changed tracked files
        }
        // A tracked file with uncommitted edits: hash its working-tree bytes.
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

    /// Whether `abs` (inside the worktree) is clean and tracked — `git status`
    /// reports nothing for it (a dirty or untracked path is non-empty).
    fn is_clean(&self, abs: &Path) -> Result<bool, String> {
        let out = git_capture(
            &["status", "--porcelain", "--", &abs.to_string_lossy()],
            None,
        )?;
        Ok(out.trim().is_empty())
    }

    /// Whether git tracks `abs` (or, for a directory, anything under it) —
    /// `git ls-files` lists a path only if it's in the index (staged or committed),
    /// so an empty result means untracked. Used to reject untracked paths a clean
    /// check can't catch (a path with uncommitted changes is "dirty" either way).
    fn is_tracked(&self, abs: &Path) -> Result<bool, String> {
        let out = git_capture(&["ls-files", "--", &abs.to_string_lossy()], None)?;
        Ok(!out.trim().is_empty())
    }

    /// The `(mode, oid)` git records for a clean tracked path, read from `HEAD`
    /// (`ls-tree` prints `<mode> <type> <hash>\t<name>`). No file is read.
    fn tracked_entry(
        &self,
        abs: &Path,
        rel: &Path,
    ) -> Result<(gix::objs::tree::EntryMode, gix::ObjectId), String> {
        use gix::objs::tree::EntryKind;

        if rel.as_os_str().is_empty() {
            let out = git_capture(&["rev-parse", "HEAD^{tree}"], None)?;
            return Ok((EntryKind::Tree.into(), parse_oid(out.trim())?));
        }

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

    /// Hash a tracked directory `abs` (worktree-relative `rel`) with uncommitted
    /// edits into the repo, re-reading only its changed files. We copy the real
    /// index to a throwaway one (inheriting its stat-cache), `git add -u` the
    /// directory there, then `write-tree --prefix` to read back just that subtree.
    /// `-u` restages only already-tracked files (picking up edits and deletions)
    /// and skips untracked ones, so the result tree holds exactly what git knows —
    /// the nix-flakes rule (see [`git_ingest`]).
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
            git_capture(&["add", "-u", "--", &abs.to_string_lossy()], Some(&tmp))?;
            let tree = if rel.as_os_str().is_empty() {
                git_capture(&["write-tree"], Some(&tmp))?
            } else {
                let prefix = format!("--prefix={}/", rel.to_string_lossy());
                git_capture(&["write-tree", &prefix], Some(&tmp))?
            };
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

/// Fetch object `hash` (and its closure) from the `caos` remote into the local
/// repo.
///
/// `fetch.negotiationAlgorithm=noop` makes git send *no* "have" lines, so the
/// negotiation is a single round. That's deliberate: the server's smart-HTTP
/// delegate returns an empty body partway through a *multi-round* negotiation —
/// which a client repo with real history (many refs/commits) triggers — and the
/// fetch then dies with "the remote end hung up unexpectedly". The client and the
/// caos server share no history anyway, so suppressing haves costs nothing here.
fn fetch_object(hash: &str) -> Result<(), String> {
    run_git(&[
        "-c",
        "fetch.negotiationAlgorithm=noop",
        "fetch",
        "--quiet",
        CAOS_REMOTE,
        hash,
    ])
    .map_err(|e| format!("fetching {hash} from {CAOS_REMOTE}: {e}"))
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

/// Fetch object `hash` and write it to `target` (blob → file, tree → directory,
/// commit → a file holding the raw commit object, kind-tagged so the path stays
/// distinguishable from a blob).
pub fn fetch_and_materialize(t: &dyn Transport, target: &Path, hash: &str) -> Result<(), String> {
    let (kind, content) = t.get_object(hash)?;

    // The transport returns the object's true type, so no guessing.
    if kind == "tree" {
        let tree = gix::objs::TreeRef::from_bytes(&content, gix::hash::Kind::Sha1)
            .map_err(|e| format!("malformed tree {hash}: {e}"))?;
        write_tree(t, target, hash, &tree)
    } else {
        write_file(target, hash, &kind, &content)
    }
}

/// Fetch object `hash` and check it out at `target` as an ordinary, faithful
/// on-disk node for use on the host, dispatched on its git tree-entry `kind`:
/// a tree → a `0755` directory whose entries are checked out the same way,
/// recursively; a symlink → a real symlink to the recorded target; a blob → a
/// `0644` file holding its bytes, or `0755` for git's executable blob.
///
/// Unlike [`fetch_and_materialize`] — the worker's CAS form, which leaves
/// owner-only placeholders and read-only, hash-tagged content and collapses every
/// non-tree to a plain file — this is a plain `git checkout`-style tree: no
/// placeholders, no xattrs, normal rw modes, symlinks and the exec bit preserved.
/// It's what `caos-cli run` uses so the result is readable and editable on disk.
fn checkout(
    t: &dyn Transport,
    target: &Path,
    hash: &str,
    kind: gix::objs::tree::EntryKind,
) -> Result<(), String> {
    use gix::objs::tree::EntryKind;
    match kind {
        EntryKind::Tree => {
            let (_, content) = t.get_object(hash)?;
            let tree = gix::objs::TreeRef::from_bytes(&content, gix::hash::Kind::Sha1)
                .map_err(|e| format!("malformed tree {hash}: {e}"))?;
            atomically(target, |tmp| {
                std::fs::create_dir(tmp).map_err(|e| format!("creating {}: {e}", tmp.display()))?;
                for entry in &tree.entries {
                    let child = tmp.join(OsStr::from_bytes(entry.filename));
                    checkout(t, &child, &entry.oid.to_string(), entry.mode.kind())?;
                }
                // Normal traversable/writable directory.
                set_mode(tmp, 0o755)
            })
        }
        EntryKind::Link => {
            // A git symlink is a blob holding the link target; recreate the symlink.
            let (_, content) = t.get_object(hash)?;
            let dest = PathBuf::from(OsStr::from_bytes(&content));
            atomically(target, |tmp| {
                std::os::unix::fs::symlink(&dest, tmp)
                    .map_err(|e| format!("linking {} -> {}: {e}", tmp.display(), dest.display()))
            })
        }
        EntryKind::Blob | EntryKind::BlobExecutable => {
            let (_, content) = t.get_object(hash)?;
            atomically(target, |tmp| {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(tmp)
                    .map_err(|e| format!("creating {}: {e}", tmp.display()))?;
                file.write_all(&content)
                    .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
                // Normal rw file, preserving git's executable bit.
                let mode = if kind == EntryKind::BlobExecutable {
                    0o755
                } else {
                    0o644
                };
                set_mode(tmp, mode)
            })
        }
        // Gitlinks (submodule commits) never appear in trees caos builds.
        EntryKind::Commit => Err(format!("cannot check out a gitlink ({hash}) to disk")),
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
/// (`/cas/foo`, never `/cas/foo/bar` or a path outside `/cas`) that doesn't
/// exist yet. A CAS path is **single-assignment**: it's recorded once
/// (`get-hash`/`put`/`map-then`) and referenced thereafter — without this
/// check, `rename(2)` would silently replace an existing file (clobbering,
/// e.g., the promise placeholder a `map-then` sealed at `/cas/out`).
fn validate_target(cas: &Path, path: &str) -> Result<PathBuf, String> {
    let target = PathBuf::from(path);

    if target.parent() != Some(cas) || target.file_name().is_none() {
        return Err(format!(
            "path must be a direct child of {} (e.g. {}/foo), got: {path}",
            cas.display(),
            cas.display()
        ));
    }
    // symlink_metadata so a dangling symlink counts as occupied too.
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(format!(
            "{path} already exists; a CAS path is recorded once — write to a fresh path"
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

/// Non-tree object → atomically write `data` to `target`, tagged with `hash`.
/// A blob's shape implies its kind; a `commit` (its `data` the raw commit
/// object: headers, blank line, message) is additionally kind-tagged so the
/// loaded file stays distinguishable from a blob (see [`KIND_XATTR`]).
fn write_file(target: &Path, hash: &str, kind: &str, data: &[u8]) -> Result<(), String> {
    atomically(target, |tmp| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(tmp)
            .map_err(|e| format!("creating {}: {e}", tmp.display()))?;
        file.write_all(data)
            .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        set_hash(tmp, hash.as_bytes())?;
        if kind == "commit" {
            xattr::set(tmp, KIND_XATTR, b"commit")
                .map_err(|e| format!("setting {KIND_XATTR} on {}: {e}", tmp.display()))?;
        }
        // Fetched content: world-readable, writable by no one.
        set_mode(tmp, MODE_FETCHED_FILE)
    })
}

/// Tree → atomically create `target` as a directory tagged with `hash`, holding
/// one empty placeholder per entry (a directory for subtrees, a file otherwise),
/// each tagged with that entry's oid so it can later be expanded with `get`.
///
/// Symlink entries are the exception: a git symlink is a blob holding its target
/// path, so there is nothing to lazily load — its content *is* the link. We fetch
/// that tiny blob now and recreate the real symlink, so the worker sees a link as
/// a link (a symlink can't carry the placeholder/loaded mode or a hash xattr
/// anyway — the OS fixes its mode and xattr ops would follow it to the target).
fn write_tree(
    t: &dyn Transport,
    target: &Path,
    hash: &str,
    tree: &gix::objs::TreeRef,
) -> Result<(), String> {
    use gix::objs::tree::EntryKind;
    atomically(target, |tmp| {
        std::fs::create_dir(tmp).map_err(|e| format!("creating {}: {e}", tmp.display()))?;
        set_hash(tmp, hash.as_bytes())?;
        for entry in &tree.entries {
            let child = tmp.join(OsStr::from_bytes(entry.filename));
            // A symlink is fully materialized here, not left as a placeholder.
            if entry.mode.kind() == EntryKind::Link {
                let (_, dest) = t.get_object(&entry.oid.to_string())?;
                std::os::unix::fs::symlink(OsStr::from_bytes(&dest), &child).map_err(|e| {
                    format!(
                        "linking {} -> {}: {e}",
                        child.display(),
                        String::from_utf8_lossy(&dest)
                    )
                })?;
                continue;
            }
            // Each child is a placeholder: it records its hash but holds no
            // content until expanded with `get`, so it stays owner-only — the
            // worker mustn't read what it hasn't fetched. A commit entry (a
            // gitlink, e.g. a commit-valued arg) is a file placeholder whose
            // kind can't be implied by shape, so it's kind-tagged.
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
            if entry.mode.kind() == EntryKind::Commit {
                xattr::set(&child, KIND_XATTR, b"commit")
                    .map_err(|e| format!("setting {KIND_XATTR} on {}: {e}", child.display()))?;
            }
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
/// owner-only (the placeholder mode). A `promise` (the continuation
/// `caos map-then`/`run-then` records at `/cas/out`) or a `commit` (a minted
/// commit, see [`put_commit`]) is a file placeholder additionally tagged with
/// its kind ([`KIND_XATTR`]), since neither's shape can imply it.
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
            "promise" | "commit" => {
                std::fs::File::create(tmp)
                    .map_err(|e| format!("creating {}: {e}", tmp.display()))?;
                xattr::set(tmp, KIND_XATTR, kind.as_bytes())
                    .map_err(|e| format!("setting {KIND_XATTR} on {}: {e}", tmp.display()))?;
                MODE_PLACEHOLDER_FILE
            }
            other => return Err(format!("unknown result type {other:?}")),
        };
        set_hash(tmp, hash.as_bytes())?;
        set_mode(tmp, mode)
    })
}

/// The result kind recorded at `path`: its [`KIND_XATTR`] if present (a promise
/// placeholder), else implied by shape — a directory is a tree, a file a blob.
/// What the runner reports for `/cas/out`.
pub fn result_kind(path: &Path) -> Result<String, String> {
    if let Ok(Some(kind)) = xattr::get(path, KIND_XATTR) {
        return String::from_utf8(kind)
            .map_err(|e| format!("invalid {KIND_XATTR} on {}: {e}", path.display()));
    }
    Ok(if path.is_dir() { "tree" } else { "blob" }.to_string())
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
    // A symlink is materialized in full the moment its tree is written (its
    // target path is its only content), so there is nothing to load and nothing
    // to descend into — and we must not follow it, since `is_dir`/`read_dir`
    // below would otherwise traverse the link's destination.
    if std::fs::symlink_metadata(target)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Ok(());
    }
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

    let (_, oid) = store(t, Some(&cas_real), Path::new(src))?;
    fetch_and_materialize(t, &target, &oid.to_string())
}

/// `put-commit <src-file> <cas-path>` — store `<src-file>`'s bytes as a git
/// **commit** object and record it at `<cas-path>` (a direct child of the CAS,
/// kind-tagged `commit`), printing the commit's hash. The file must hold a
/// valid raw commit — `tree <hash>`, `parent <hash>`*, `author`/`committer`
/// lines, a blank line, the message — validated here (and again server-side).
/// This is how a worker *mints* a commit: write one at `/cas/out` to return
/// `commit <hash>` as the run's result, or at a fresh path to reference from
/// further calls (it's a commit-typed path, so `--name:@=` and `:commit=` args
/// both carry it as a gitlink).
pub fn put_commit(t: &dyn Transport, src: &str, dst: &str) -> Result<(), String> {
    let cas = cas_dir();
    let target = validate_target(&cas, dst)?;
    probe_xattr(&cas)?;

    let bytes = std::fs::read(src).map_err(|e| format!("{src}: {e}"))?;
    gix::objs::CommitRef::from_bytes(&bytes, gix::hash::Kind::Sha1)
        .map_err(|e| format!("{src} is not a valid commit: {e}"))?;
    let oid = post_object(t, "commit", &bytes)?;
    write_placeholder(&target, "commit", &oid.to_string())?;
    // The minted commit's hash — the caller's handle (e.g. the next parent).
    println!("{oid}");
    Ok(())
}

/// `hash <path>` — print the git hash recorded on a CAS path. The setuid route
/// to a path's identity: a worker minting a commit needs its parent's *hash*
/// (for the `parent` line), and an unfetched placeholder's xattr is unreadable
/// to the unprivileged worker directly.
pub fn cas_hash(path: &str) -> Result<(), String> {
    let cas = cas_dir();
    let target = validate_descendant(&cas, path)?;
    println!("{}", read_hash(&target)?);
    Ok(())
}

/// Recursively store `path` via the transport, returning the git tree entry
/// (mode + oid) that refers to it. `cas_real` is the canonical CAS root, used to
/// reuse the recorded hash of a symlink that resolves into the CAS; pass `None`
/// (e.g. `import-image`) to always store symlinks as git symlinks.
fn store(
    t: &dyn Transport,
    cas_real: Option<&Path>,
    path: &Path,
) -> Result<(gix::objs::tree::EntryMode, gix::ObjectId), String> {
    use gix::objs::tree::EntryKind;

    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let ft = meta.file_type();

    if ft.is_symlink() {
        // A symlink that resolves into the CAS: reuse the hash recorded there
        // instead of re-reading the target.
        if let Some(cas_real) = cas_real {
            if let Ok(canon) = path.canonicalize() {
                if canon != cas_real && canon.starts_with(cas_real) {
                    return cas_entry(&canon);
                }
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
/// there rather than re-reading content, with the mode following its shape — a
/// directory is a tree, a file a blob, unless a [`KIND_XATTR`] says otherwise
/// (a commit-valued path becomes a gitlink entry, so a commit passes through
/// args without being mistaken for a blob). Shared by `store` (symlinks into
/// the CAS) and `build_arg_entries` (CAS-path arg values).
fn cas_entry(canon: &Path) -> Result<(gix::objs::tree::EntryMode, gix::ObjectId), String> {
    use gix::objs::tree::EntryKind;
    let kind = if canon.is_dir() {
        EntryKind::Tree
    } else if result_kind(canon)? == "commit" {
        EntryKind::Commit
    } else {
        EntryKind::Blob
    };
    Ok((kind.into(), parse_oid(&read_hash(canon)?)?))
}

/// `import-image <docker-archive>` — store a docker-archive image (the kind `nix
/// build .#caos-*-docker` / `docker save` produce) into caos in git-docker form:
/// a tree holding `config.json` (the image config, verbatim) and one `layer<NN>`
/// subtree per layer (the layer tar's extracted filesystem). Prints the stored
/// git-docker tree's hash, which a caller can `run` (the server converts it back
/// into a real image) or assemble into a larger tree (the built-ins library
/// does this). Nothing is materialized locally — there is no `/cas` on the host.
///
/// Only the layer *contents* are captured (files, the exec bit, and symlinks);
/// mtimes/owners are dropped, which is fine — the server re-tars the trees
/// deterministically and generates the diff_ids itself.
pub fn import_image(t: &dyn Transport, archive: &str, base: Option<&str>) -> Result<(), String> {
    use gix::objs::tree::{Entry, EntryKind};

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

        // Optional `base`: a `docker://<ref>` the server stacks these (delta)
        // layers on top of at convert time, pulling the base from its source
        // registry. So a heavy stock base (e.g. a toolchain) never enters git —
        // only this archive's own layers do.
        if let Some(base) = base {
            let base = base.trim();
            if base.is_empty() {
                return Err("--base ref is empty".to_string());
            }
            entries.push(Entry {
                mode: EntryKind::Blob.into(),
                filename: "base".as_bytes().to_vec().into(),
                oid: post_object(t, "blob", base.as_bytes())?,
            });
        }

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
            let (_, oid) = store(t, None, &layer_dir)?;
            entries.push(Entry {
                mode: EntryKind::Tree.into(),
                filename: format!("layer{i:02}").into_bytes().into(),
                oid,
            });
            eprintln!("imported layer{i:02} from {layer_path}");
        }

        let image_oid = post_tree(t, entries)?;
        // Print the stored git-docker tree's hash — the caller's handle to it.
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

/// The per-arg tree entries that make up an args tree — `run`/`curry` merge call
/// args with a curry node's bound args, then `post_tree` the result.
///
/// Each `--name[:type]=value` becomes a tree entry `name` (see [`parse_kv`]):
/// * `--name=value` — a literal, stored verbatim as a blob;
/// * `--name:@=path` inside the CAS — references the object that path was
///   materialized from (its recorded hash). Only when `cas` is `Some` (the
///   worker); the CLI passes `None`, so every path is a host path;
/// * `--name:@=path` elsewhere — a host path, ingested via the transport (the git
///   transport ingests it from the working repo, and only if git tracks it — see
///   [`GitTransport::ingest_path`]); a worker has no host filesystem, so this is
///   an error there;
/// * `--name:commit=value` — a commit, passed unpeeled as a gitlink entry (see
///   [`resolve_commit_arg`]).
fn build_arg_entries(
    t: &dyn Transport,
    cas: Option<&Path>,
    kvs: &[String],
) -> Result<Vec<gix::objs::tree::Entry>, String> {
    use gix::objs::tree::{Entry, EntryKind};

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
            ArgValue::Path(p) if cas.is_some_and(|c| Path::new(p).starts_with(c)) => {
                let cas = cas.expect("checked is_some_and above");
                let canon = Path::new(p)
                    .canonicalize()
                    .map_err(|e| format!("{p}: {e}"))?;
                let cas_real = cas
                    .canonicalize()
                    .map_err(|e| format!("CAS directory {}: {e}", cas.display()))?;
                if !canon.starts_with(&cas_real) {
                    return Err(format!("{p} resolves outside {}", cas.display()));
                }
                cas_entry(&canon)?
            }
            // `--name:@=/cas/std/<name>` off-worker — the CLI has no `/cas`, but
            // a builtin ref is meaningful there too (e.g. currying the runner
            // image into the rustc builder): resolve it against the published
            // library, the same vocabulary the image argument uses.
            ArgValue::Path(p) if cas.is_none() && p.starts_with(&std_arg_prefix()) => {
                let name = &p[std_arg_prefix().len()..];
                let hash = resolve_std_image(t, name)?;
                (EntryKind::Tree.into(), parse_oid(&hash)?)
            }
            // `--name:@=path` elsewhere — ingest a host path (git transport only;
            // the worker has no host filesystem, so it errors clearly).
            ArgValue::Path(p) => t.ingest_path(p)?.ok_or_else(|| {
                format!("`{name}`: {p:?} is a host path, but this client only reads /cas paths")
            })?,
            // `--name:commit=value` — a commit, unpeeled, as a gitlink entry.
            ArgValue::Commit(v) => (
                EntryKind::Commit.into(),
                resolve_commit_arg(t, cas, v).map_err(|e| format!("`{name}`: {e}"))?,
            ),
        };

        entries.push(Entry {
            mode,
            filename: name.as_bytes().to_vec().into(),
            oid,
        });
    }

    Ok(entries)
}

/// Resolve a `--name:commit=value` argument to a commit id — the explicit,
/// **unpeeled** form (the default resolutions peel a commit to its tree, e.g.
/// [`resolve_ref`], which image refs depend on; a commit-typed arg must stay a
/// commit). Accepted values:
///
/// * a bare commit hash — verified to name a commit (both clients);
/// * a `/cas` path recorded as a commit (worker) — e.g. a commit-valued arg or
///   a freshly minted [`put_commit`] result, passed on by reference;
/// * anything else on the CLI — a revspec (`HEAD`, a branch, …) resolved in the
///   working repo via [`Transport::resolve_revspec`].
///
/// The commit rides in the args tree as a *gitlink* entry (mode 160000), which
/// git's reachability does **not** traverse — so unlike every other arg it is
/// not carried by the request's own push. `ensure_pushed` ships the commit's
/// closure separately (a no-op on the worker's HTTP transport, where the object
/// is already server-side).
fn resolve_commit_arg(
    t: &dyn Transport,
    cas: Option<&Path>,
    value: &str,
) -> Result<gix::ObjectId, String> {
    let oid = if is_hex_hash(value) {
        let (kind, _) = t.get_object(value)?;
        if kind != "commit" {
            return Err(format!("{value} is a {kind}, not a commit"));
        }
        parse_oid(value)?
    } else if cas.is_some_and(|c| Path::new(value).starts_with(c)) {
        let cas = cas.expect("checked is_some_and above");
        let canon = Path::new(value)
            .canonicalize()
            .map_err(|e| format!("{value}: {e}"))?;
        let cas_real = cas
            .canonicalize()
            .map_err(|e| format!("CAS directory {}: {e}", cas.display()))?;
        if !canon.starts_with(&cas_real) {
            return Err(format!("{value} resolves outside {}", cas.display()));
        }
        let kind = result_kind(&canon)?;
        if kind != "commit" {
            return Err(format!("{value} is recorded as a {kind}, not a commit"));
        }
        parse_oid(&read_hash(&canon)?)?
    } else {
        t.resolve_revspec(value)?.ok_or_else(|| {
            format!("{value:?} is not a commit hash or /cas path (a worker has no repo to resolve a revspec against)")
        })?
    };
    // Gitlinks aren't reachability-traversed, so push the commit's own closure.
    t.ensure_pushed(&oid.to_string())?;
    Ok(oid)
}

/// A parsed `--name[:type]=value` argument value. The type marker lives in the
/// operator, not the value, so the value is unconstrained (it may start with
/// anything, no escaping). Bare `=` is a literal; `:@=` marks a path; `:commit=`
/// marks a commit. The grammar
/// is extensible — a new type adds a variant here and a case in [`parse_kv`].
enum ArgValue<'a> {
    /// `--name=value` — the value verbatim, stored as a blob.
    Literal(&'a str),
    /// `--name:@=path` — the value names a filesystem path to resolve/ingest.
    Path(&'a str),
    /// `--name:commit=value` — the value names a *commit*, passed **unpeeled**
    /// as a gitlink entry: a commit hash, a `/cas` path recorded as a commit
    /// (worker), or a revspec like `HEAD` (CLI). The explicit opt-in exists
    /// because the default forms peel commits to trees (which image refs rely
    /// on); see [`resolve_commit_arg`].
    Commit(&'a str),
}

/// Split a `--name[:type]=value` argument into its name and typed value,
/// validating that the name is a single path component (it becomes a tree-entry
/// filename). The types are `@` (a path) and `commit`; bare is a literal.
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
        Some((name, "commit")) => (name, ArgValue::Commit(value)),
        Some((_, ty)) => {
            return Err(format!(
                "unknown argument type {ty:?} in {kv:?}; use --name=value (literal), \
                 --name:@=value (path), or --name:commit=value (commit)"
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

/// Resolve curry layers, build the args tree, bundle + push the request, and run
/// it — the CLI's blocking run (the worker never triggers compute; its sub-runs
/// are continuations the server resolves). Returns the server's
/// `(kind, result-hash)`. `cas` is `None` here: every path arg is a host path to
/// ingest.
fn run_request(
    t: &dyn Transport,
    image: &str,
    cas: Option<&Path>,
    kvs: &[String],
) -> Result<(String, String), String> {
    let req = prepare_request(t, image, cas, kvs)?;
    // Trigger compute; the server runs the container and returns the result's
    // "<type> <hash>" (and, for a top-level run, pins refs/caos/res/<req> at it).
    request_compute(&t.server_url()?, &req)
}

/// Everything in [`run_request`] up to (and including) getting the request onto
/// the server, returning the request id. Split out so a caller can trigger the
/// blocking compute itself — `chat` runs [`request_compute`] on its own thread
/// (it needs only the request id and the server URL, both plain strings) while
/// it watches the turn's progress ref from the main one.
fn prepare_request(
    t: &dyn Transport,
    image: &str,
    cas: Option<&Path>,
    kvs: &[String],
) -> Result<String, String> {
    // Expand any curry layers: pull the underlying image out and collect the args
    // bound into it. The image is folded into the args tree below, so the server
    // only ever sees a plain args tree.
    let (image, bound) = unwrap_curry(t, image)?;

    // Build the call's args, then merge them over the bound ones (call wins).
    let call = build_arg_entries(t, cas, kvs)?;
    // The worker (image) rides *in* the args tree under the reserved `image`
    // entry, rather than as a sibling of `args` in the request. So a computation
    // is identified entirely by its args (an executor can match on the worker
    // alongside the rest), and a worker — which sees its args at `/cas/args` —
    // reaches its own image at `/cas/args/image` to call itself. Merged last so
    // the reserved name wins over any like-named user arg.
    //
    // A git-docker image *is* a git tree, so we reference it by that tree (the
    // entry's oid is the image tree): the image then travels inside the request's
    // own object graph — no separate push — and materializes at `/cas/args/image`
    // as a real directory whose recorded hash is the image, so recursion can pass
    // that path straight to `caos run`. A `docker://` ref has no git object to
    // embed, so it rides as a blob naming the registry ref.
    let image_entry = image_arg_entry(t, &image)?;
    let args_tree = post_tree(
        t,
        merge_entries(merge_entries(bound, call), vec![image_entry]),
    )?;

    // The built-in tree (`std`): inherited from CAOS_STD inside a worker, or
    // resolved from the `refs/caos/std` ref at the top. Part of the request so the
    // server keys on it and threads it down (materialized at /cas/std).
    let std = run_std()?;
    // The cache-busting salt (empty by default), threaded like std.
    let salt = run_salt();

    // Bundle the request as a content-addressed object {args, std, salt} (the
    // worker image is inside `args`); its hash is the request id (and the server's
    // cache key). Get it onto the server — a no-op POST-as-you-go for the HTTP
    // transport, a push for the git one. The push carries the whole graph
    // reachable from the request, which now includes any embedded git-image tree,
    // so the image lands on the server without a separate push.
    let req = build_request(t, &args_tree, &std, &salt)?;
    t.ensure_pushed(&req.to_string())?;
    Ok(req.to_string())
}

/// `map-then <in> -- [--map=<image>] [--then=<image>]` — the *worker* form: record a
/// continuation `{in, map?, then?}` as this worker's result at
/// `/cas/out`, fetching and running nothing. The worker then exits, and the
/// *server* resolves the continuation — `map` over each child of `in` in
/// parallel, then `then(--in, --children)` — with no worker slot held (see
/// `design/map-then.md`). So `caos map-then` is a tail call: it produces `/cas/out`
/// itself and must be the worker's final act. At least one of `--map`/`--then`
/// is required; each names an image (a `/cas` path, resolved to the hash
/// recorded on it, or a git/curry hash or `docker://` ref, passed through).
/// (The user-facing CLI's blocking run is [`cli_run`]; the single-valued form
/// is [`caos_run_then`].)
pub fn caos_map_then(t: &dyn Transport, input: &str, kvs: &[String]) -> Result<(), String> {
    record_continuation(t, "map-then", input, kvs, &["map", "then"], |given| {
        if given.is_empty() {
            return Err("`map-then` needs --map and/or --then".to_string());
        }
        Ok(())
    })
}

/// `run-then <in> -- --run=<image> [--then=<image>]` — the single-valued
/// [`caos_map_then`]: record a continuation `{in, run, then?}` as this worker's
/// result at `/cas/out` and exit. The server runs `run(--in=<in>)` once,
/// yielding R; with `--then` the request's result is `then(--in=<in>,
/// --result=<R>)` (symmetric with map-then's `--in`/`--children`), else R
/// itself — so `run` with no `then` is a plain tail call to `run`. `--run` is
/// required (a bare tail call to one image); `--map` doesn't belong here —
/// `map` and `run` are mutually exclusive, which this surface enforces
/// client-side. Image refs resolve exactly as in `map-then`.
pub fn caos_run_then(t: &dyn Transport, input: &str, kvs: &[String]) -> Result<(), String> {
    record_continuation(t, "run-then", input, kvs, &["run", "then"], |given| {
        if !given.contains(&"run") {
            return Err("`run-then` needs --run (with an optional --then)".to_string());
        }
        Ok(())
    })
}

/// Shared body of [`caos_map_then`] / [`caos_run_then`]: record a continuation
/// `{in, <images>}` over `input` as this worker's result at `/cas/out` (a
/// `promise` placeholder the server resolves once the job is posted). `allowed`
/// names the image-valued entries this verb accepts — the surface split is the
/// client-side mutual exclusion of `map` and `run` — and `check` validates the
/// set actually given, before anything is sealed.
fn record_continuation(
    t: &dyn Transport,
    verb: &str,
    input: &str,
    kvs: &[String],
    allowed: &[&'static str],
    check: impl FnOnce(&[&str]) -> Result<(), String>,
) -> Result<(), String> {
    use gix::objs::tree::{Entry, EntryKind};

    let cas = cas_dir();
    probe_xattr(&cas)?;
    let out = cas.join("out");
    if std::fs::symlink_metadata(&out).is_ok() {
        return Err(format!(
            "{} already exists; `caos {verb}` records the worker's result, so it must \
             be the worker's final act",
            out.display()
        ));
    }

    // `in` is the data node the continuation is over: an existing CAS path,
    // referenced as a real tree entry (mode + recorded hash) so the server knows
    // its shape without fetching anything.
    let in_path = validate_descendant(&cas, input)?;
    let (in_mode, in_oid) = cas_entry(&in_path)?;
    let mut entries = vec![Entry {
        mode: in_mode,
        filename: b"in".to_vec().into(),
        oid: in_oid,
    }];

    let mut given: Vec<&str> = Vec::new();
    for kv in kvs {
        let (name, value) = parse_kv(kv)?;
        let Some(&name) = allowed.iter().find(|&&a| a == name) else {
            let flags = allowed
                .iter()
                .map(|a| format!("--{a}"))
                .collect::<Vec<_>>()
                .join(" and ");
            return Err(format!(
                "`{verb}` takes only {flags} (each an image ref), got --{name}"
            ));
        };
        if given.contains(&name) {
            return Err(format!("--{name} given twice"));
        }
        // Literal or path form, the value names an image; resolve_run_image
        // handles all the shapes (a /cas path, a bare hash, docker://).
        let image = match value {
            ArgValue::Literal(v) => v,
            ArgValue::Path(p) => p,
            ArgValue::Commit(_) => {
                return Err(format!("--{name} names an image, not a commit"));
            }
        };
        let resolved = resolve_run_image(&cas, image)?;
        entries.push(Entry {
            mode: EntryKind::Blob.into(),
            filename: name.as_bytes().to_vec().into(),
            oid: post_object(t, "blob", resolved.as_bytes())?,
        });
        given.push(name);
    }
    check(&given)?;

    let continuation = post_tree(t, entries)?;
    write_placeholder(&out, "promise", &continuation.to_string())
}

/// `run <image | /cas/std/<name>> [output] -- [--name=value | --name:@=path ...]`
/// — the *CLI* form. `<output>`, if given, is any path on the host; the whole
/// result tree is checked out there in full as ordinary rw files. If `<output>`
/// is omitted and the result is a file, its bytes are written to stdout — with a
/// trailing newline added when stdout is a terminal and the bytes don't already
/// end in one, so the shell prompt lands on its own line without corrupting a
/// pipe or redirect. A tree has no single stream to print, so an output path is
/// required for one. A `commit` result behaves like a file whose bytes are the
/// raw commit object (headers, blank line, message) — streamed or written to
/// `<output>` as such; fetch the real object by hash (`git fetch caos <hash>`)
/// when you want the commit itself. There
/// is no `/cas` here: path-valued args are host paths the transport ingests, and
/// `<image>` is a `docker://` ref, a bare hash, or a `/cas/std/<name>` builtin
/// (resolved against the published library).
pub fn cli_run(
    t: &dyn Transport,
    image: &str,
    output: Option<&str>,
    kvs: &[String],
) -> Result<(), String> {
    let image = resolve_cli_image(t, image)?;
    let (kind, result) = run_request(t, &image, None, kvs)?;

    let Some(output) = output else {
        // No output path: stream a file result to stdout. A tree has no single
        // stream to print, so it needs an explicit path to check out to.
        if kind == "tree" {
            return Err("result is a tree; pass an <output> path to check it out".to_string());
        }
        let (_, content) = t.get_object(&result)?;
        let mut out = std::io::stdout();
        out.write_all(&content)
            .map_err(|e| format!("writing to stdout: {e}"))?;
        // On a terminal, end on a newline so the prompt doesn't collide with the
        // output; when piped or redirected, leave the bytes exactly as produced.
        if out.is_terminal() && !content.ends_with(b"\n") {
            out.write_all(b"\n")
                .map_err(|e| format!("writing to stdout: {e}"))?;
        }
        return Ok(());
    };

    // Check the result out in full as ordinary rw files — the object and, for a
    // tree, every descendant — so it's readable and editable on the host.
    let target = PathBuf::from(output);
    let root = if kind == "tree" {
        gix::objs::tree::EntryKind::Tree
    } else {
        gix::objs::tree::EntryKind::Blob
    };
    checkout(t, &target, &result, root)
}

/// The reserved `image` entry for an args tree, carrying the worker image `image`
/// (a resolved ref: `docker://…` or a git-image hash). A git-docker image *is* a
/// git tree, so it rides embedded — the entry references that tree directly, so
/// the image travels inside the request's object graph and materializes as a real
/// directory at `/cas/args/image`. A `docker://` ref has no git object to embed,
/// so it rides as a blob naming the registry ref.
fn image_arg_entry(t: &dyn Transport, image: &str) -> Result<gix::objs::tree::Entry, String> {
    use gix::objs::tree::{Entry, EntryKind};
    let (mode, oid) = if is_hex_hash(image) {
        (EntryKind::Tree, parse_oid(image)?)
    } else {
        (EntryKind::Blob, post_object(t, "blob", image.as_bytes())?)
    };
    Ok(Entry {
        mode: mode.into(),
        filename: b"image".to_vec().into(),
        oid,
    })
}

/// Bundle a run request as a content-addressed object: a tree `{args, std,
/// salt}` — `std`/`salt` as blobs, `args` as the args subtree (which carries the
/// worker image under its reserved `image` entry — see [`image_arg_entry`]). Its
/// hash is the request id: the server's cache key and the result-ref rendezvous.
fn build_request(
    t: &dyn Transport,
    args_tree: &gix::ObjectId,
    std: &str,
    salt: &str,
) -> Result<gix::ObjectId, String> {
    use gix::objs::tree::{Entry, EntryKind};
    let entries = vec![
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
/// [`STD_ENV`], so reuse it (threading). At the top, resolve the built-ins via
/// [`std_tree`] — the *same* path image resolution uses, so the request's `std`
/// always matches the builtins the CLI just resolved `/cas/std/<name>` against.
/// That matters because `std_tree` resolves a not-yet-local ref over `ls-remote`
/// (it never fetches the tree's closure); a local-only lookup here would leave
/// `std` empty and the worker with no `/cas/std`. Tolerate absence (no built-ins
/// published) — a worker that needs them will fail clearly.
fn run_std() -> Result<String, String> {
    if let Ok(std) = std::env::var(STD_ENV) {
        return Ok(std);
    }
    Ok(std_tree().unwrap_or_default())
}

/// The cache-busting salt for this run (see [`SALT_ENV`]): read from `CAOS_SALT`,
/// empty if unset. Read at the top of a run (the CLI); the server threads it
/// into each worker and every promise sub-run — so a whole run tree shares one.
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

/// Resolve an image argument of `caos map-then`/`caos curry` into what the server
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

/// Map a `caos-cli run` image argument that names a std builtin to its git hash,
/// leaving every other form (`docker://…`, a hash, a CAS path) untouched.
///
/// `<DEFAULT_CAS_DIR>/std/<name>` (i.e. `/cas/std/<name>`) is the path a *worker*
/// uses to reach a builtin — the server materializes the std library at
/// `/cas/std` inside the container. The CLI has no such directory, so it resolves
/// the same path directly against the published library, so one vocabulary works
/// in both places.
pub fn resolve_cli_image(t: &dyn Transport, image: &str) -> Result<String, String> {
    match image.strip_prefix(&std_arg_prefix()) {
        Some(name) => resolve_std_image(t, name),
        None => Ok(image.to_string()),
    }
}

/// The path prefix that names a builtin off-worker (`/cas/std/`).
fn std_arg_prefix() -> String {
    format!("{DEFAULT_CAS_DIR}/std/")
}

/// Resolve a std builtin `<name>` to its git-docker image hash by looking it up in
/// the published library (`refs/caos/std`).
fn resolve_std_image(t: &dyn Transport, name: &str) -> Result<String, String> {
    if name.is_empty() || name.contains('/') {
        return Err(format!(
            "a std image is {DEFAULT_CAS_DIR}/std/<name> (a single builtin name), got: {name:?}"
        ));
    }
    let std = std_tree()?;
    let entries = fetch_tree_entries(t, &std)?.ok_or_else(|| format!("std {std} is not a tree"))?;
    entries
        .iter()
        .find(|e| entry_name(e) == name.as_bytes())
        .map(|e| e.oid.to_string())
        .ok_or_else(|| format!("no builtin {name:?} in {DEFAULT_STD_REF}"))
}

/// The std library tree hash from the built-ins ref ([`STD_REF_ENV`], default
/// `refs/caos/std`), fetched from the `caos` remote if it isn't in the local repo
/// yet (the CLI may never have pulled it). This is the single resolution path for
/// both running a `/cas/std/<name>` builtin and threading `std` into a run (see
/// [`run_std`]), so the two never disagree.
fn std_tree() -> Result<String, String> {
    let refname = std::env::var(STD_REF_ENV).unwrap_or_else(|_| DEFAULT_STD_REF.to_string());
    if let Ok(hash) = resolve_ref(&refname) {
        return Ok(hash);
    }
    // Not local yet — read just the root hash from the remote's advertisement
    // (`ls-remote` — no pack negotiation, works for any object type). We
    // deliberately do NOT `git fetch` the tree: fetching a tree pulls its entire
    // reachable closure — every builtin, including the ~1.5GB `rustc` image — to
    // resolve a single name, which both wastes the network and OOM-kills the
    // server buffering that pack. Resolution needs only the std *root* tree, which
    // the HTTP transport fetches by hash on demand ([`fetch_tree_entries`]), and
    // then only the chosen builtin's subtree — so a `bash` run never pulls
    // `rustc`. We don't record the ref locally: `resolve_ref` peels by reading the
    // object, so a ref pointing at an un-fetched tree would just fail back here.
    let advertised = git_capture(&["ls-remote", CAOS_REMOTE, &refname], None)?;
    let hash = advertised
        .split_whitespace()
        .next()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| format!("{refname} not found on the `{CAOS_REMOTE}` remote"))?
        .to_string();
    Ok(hash)
}

/// `curry <image> -- [--name=value ...]` — bind arguments to `<image>`, printing
/// a ref (a git hash) to the resulting curried image. The ref can be `run` —
/// which supplies the rest of the args — or `curry`'d again, exactly like any
/// image; the binding is partial application, not a rebuilt container image.
/// This is the *worker* form: path args resolve against `/cas`. (The CLI's is
/// [`cli_curry`].)
///
/// The curried image is a small CAS tree: a `base` blob (the underlying image
/// ref), an `args` subtree (the bound args, in `build_arg_entries` shape), and a
/// [`CURRY_MARKER`] blob. Currying flattens: if `<image>` is itself curried, its
/// bindings are folded in and `base` stays a plain (docker/git) image, so the
/// result is canonical (`curry (curry img a) b` == `curry img a b`).
pub fn caos_curry(t: &dyn Transport, image: &str, kvs: &[String]) -> Result<(), String> {
    let cas = cas_dir();
    let image = resolve_run_image(&cas, image)?;
    println!("{}", curry_object(t, &image, Some(&cas), kvs)?);
    Ok(())
}

/// `curry <image> -- [--name=value ...]` — the *CLI* form of [`caos_curry`]:
/// `<image>` may be a `/cas/std/<name>` builtin, path args are host paths to
/// ingest (or `/cas/std/<name>` builtin refs), and the curried image is pushed
/// so a later `run` can use the printed ref directly.
pub fn cli_curry(t: &dyn Transport, image: &str, kvs: &[String]) -> Result<(), String> {
    let image = resolve_cli_image(t, image)?;
    let curried = curry_object(t, &image, None, kvs)?;
    t.ensure_pushed(&curried.to_string())?;
    if is_hex_hash(&image) {
        t.ensure_pushed(&image)?;
    }
    println!("{curried}");
    Ok(())
}

/// Build (and store) a curry node over the resolved `image`: the shared body of
/// [`caos_curry`] / [`cli_curry`]. `cas` decides how path args resolve, exactly
/// as in [`run_request`].
fn curry_object(
    t: &dyn Transport,
    image: &str,
    cas: Option<&Path>,
    kvs: &[String],
) -> Result<gix::ObjectId, String> {
    use gix::objs::tree::{Entry, EntryKind};

    let (image, bound) = unwrap_curry(t, image)?;

    // New bindings override any already bound to the same name.
    let args = merge_entries(bound, build_arg_entries(t, cas, kvs)?);
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
    post_tree(t, entries)
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
/// server runs the container (resolving any promise it leaves behind) and
/// replies with the final `"<type> <hash>"`.
fn request_compute(base: &str, req: &str) -> Result<(String, String), String> {
    let url = format!("{}/run?req={}", base.trim_end_matches('/'), req);
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

/// Program name from `argv[0]` (`caos`/`caos-cli` in the image or build tree),
/// for diagnostics and usage.
pub fn prog_name(args: &[String]) -> &str {
    args.first()
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .unwrap_or("caos")
}
