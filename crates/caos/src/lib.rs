//! caos client library: the engine shared by the worker, line-oriented CLI,
//! and richer clients such as `caos tui`.
//!
//! The package provides two binaries (see the crate's `bin/`):
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
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use gix::objs::WriteTo;

pub mod chat;
pub use chat::{cli_chat, cli_talk};

mod eval;
pub use eval::cli_eval_path;

/// `run-tool <script | name> [--name=value ...]` — run a caos-tool by hand: fire
/// the tool script as a caos job over this repo's tree, exactly what an
/// agent's tool invocation does. The bash worker gets the tracked worktree
/// (dirty edits included) as `--in`, plus the extra args verbatim. A bare
/// name resolves to `caos-tools/<name>.sh` — the project's tree-defined
/// tool directory.
///
/// NOTHING IS MATERIALIZED. The result stays on the server and only its hash
/// comes back, plus whatever the report conventions below print — a handful of
/// small blobs, read one object at a time. It used to check the whole result
/// out under `.caos-dev/tool-<name>`, which for `build` meant fetching and
/// writing a 218 MB stack image nobody reads: ~19s of a 38s run on a one-line
/// worker edit. When you do want the tree — a failing test's inner stack logs,
/// say — `caos-cli get <hash> <path>` checks it out.
///
/// Conventions on the result: a BLOB result is printed verbatim (a tool whose
/// answer is text, like `test-result`), and a `report` file is printed (a
/// FAILED banner fails the command). Everything else just gets the printed
/// result hash. Exactly what an agent is shown for the same call — the report
/// is the tool's whole answer to both, so neither reader sees more than the
/// other.
pub fn cli_run_tool(t: &dyn Transport, args: &[String]) -> Result<(), String> {
    let (tool, kvs) = match args {
        [tool, kvs @ ..] => (tool, kvs),
        _ => return Err("usage: run-tool <script | name> [--name=value ...]".to_string()),
    };
    let script = if tool.contains('/') || tool.ends_with(".sh") {
        tool.clone()
    } else {
        format!("caos-tools/{tool}.sh")
    };
    if !Path::new(&script).is_file() {
        return Err(format!("no such tool script: {script}"));
    }
    let name = Path::new(&script)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("tool")
        .to_string();

    let mut all: Vec<String> = vec![format!("--worker1:@={script}"), "--in:@=.".to_string()];
    // Do not add a `--bins` carrying the deploy's nix-built binaries. A tool
    // gets the tree under test and builds from source; handing it prebuilt host
    // binaries would couple every invocation to the deploy and let the suite
    // pass against something other than the tree it was given.
    all.extend(kvs.iter().cloned());
    // The workspace declares the image its tool scripts run on (./DEPS: `bash`).
    let image = eval::eval_workspace_dep(t, "bash")?;
    let (kind, result) = run_request(t, &image, None, None, &all, &[])?;
    // The result's identity, on stdout, so a script can thread it onward — the
    // same "<kind> <hash>" line `caos-cli run` prints.
    println!("{kind} {result}");
    eprintln!("1126 {name}: {result}");
    report_conventions(t, &name, &result)
}

/// Print a tool result's report conventions, reading ONLY the objects they
/// name: the top tree and a `report` blob, or the result itself when it is one.
/// A tool with no `report` (`build` returns an image) costs exactly one object.
fn report_conventions(t: &dyn Transport, name: &str, result: &str) -> Result<(), String> {
    let Some(entries) = fetch_tree_entries(t, result)? else {
        // Not a tree: the tool's whole answer IS the blob. Print it — the same
        // rendering the agent harness gives a blob result, so `run-tool
        // test-result <hash>` by hand shows what the agent would have seen.
        // Without this a text-valued tool prints its hash and nothing else.
        let (_, bytes) = t.get_object(result)?;
        let text = String::from_utf8_lossy(&bytes);
        eprintln!();
        eprint!("{text}");
        if !text.ends_with('\n') {
            eprintln!();
        }
        return Ok(());
    };
    let find = |entries: &[gix::objs::tree::Entry], want: &str| {
        entries
            .iter()
            .find(|e| entry_name(e) == want.as_bytes())
            .map(|e| e.oid.to_string())
    };
    let Some(report) = find(&entries, "report") else {
        return Ok(());
    };
    // The report is printed verbatim, so read the blob rather than
    // fetch_blob_string (which trims).
    let (_, text) = t.get_object(&report)?;
    let text = String::from_utf8(text).map_err(|_| format!("{name}'s report is not UTF-8"))?;
    eprintln!();
    eprint!("{text}");

    // THE REPORT IS THE WHOLE OUTPUT. There used to be a second pass here that
    // walked `results/<rec>` and dumped the full `output` of every record whose
    // verdict wasn't a PASS. Once the report itself carried each failure's tail
    // and its record hash, that printed the same failure twice — and, worse,
    // showed a human something the agent never saw, when the point of the two
    // callers is that a tool cannot behave differently depending on who ran it.
    // The full output is one `run-tool test-result <hash>` away, which the
    // report says.
    if text.contains("FAILED") {
        return Err(format!("{name} reported FAILED"));
    }
    Ok(())
}

/// `get <hash> <path>` — check an existing result out on the host, as ordinary
/// rw files. The counterpart to `run-tool` materializing nothing: it prints a
/// hash, and this is how you then read the thing. Costs only the objects the
/// working repo is missing.
pub fn cli_get(t: &dyn Transport, hash: &str, path: &str) -> Result<(), String> {
    // An EVALUABLE DIRECTORY resolves first: resolving is not running, and it
    // matters for an entry whose resolved value is DATA rather than an image —
    // `std/llm-stub` evaluates to a cargo result tree, and what a caller wants
    // from it is the produced file under `bin/`. Narrow on purpose: anything
    // else is taken as the hash it looks like, so a mistyped hash cannot
    // quietly become an ingest of a same-named directory.
    let hash = &if Path::new(hash).is_dir() {
        resolve_cli_image(t, hash)?
    } else {
        hash.to_string()
    };
    let (kind, _) = t.get_object(hash)?;
    let root = match kind.as_str() {
        "tree" => gix::objs::tree::EntryKind::Tree,
        _ => gix::objs::tree::EntryKind::Blob,
    };
    let target = PathBuf::from(path);
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
    }
    if let Err(e) = std::fs::remove_dir_all(&target) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(format!("clearing {path}: {e}"));
        }
    }
    checkout(t, &target, hash, root)?;
    eprintln!("{kind} {hash} -> {path}");
    Ok(())
}

/// Base URL of the caos server (storage + compute), e.g. `http://caos-server`.
pub const SERVER_ENV: &str = "CAOS_SERVER_URL";

/// Header carrying the ephemeral secrets store on `GET /run` (design/secrets.md),
/// out of band from the content-addressed ArgTree. Must match the server's.
const SECRETS_HEADER: &str = "X-Caos-Secrets";

/// An opaque cache-busting value mixed into every run's ArgTree — and so into its
/// arg-tree hash and cache key. Empty by default, so runs are cached purely by
/// their inputs. Like `std` it's threaded: the server injects it into each worker
/// and into every promise sub-run, so a whole run tree shares one salt. Tests set
/// it to a per-run random value, making their cache entries collision-free across
/// runs without ever touching Redis.
pub const SALT_ENV: &str = "CAOS_SALT";

/// Image-ref scheme marking an ordinary docker reference (vs. a git-image hash).
pub const DOCKER_SCHEME: &str = "docker://";

/// Marker entry naming a curry node: a CAS tree that pairs a `base` image ref
/// with an `args` subtree of bound arguments. `run`/`curry` expand it client-side
/// (merging the bound args under the call's args, then folding the base in as the
/// args' `image` entry) so the server only ever sees an ordinary args tree. The
/// marker lets it be told apart from a
/// git-docker image tree, which it otherwise resembles. See `unwrap_curry`.
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
/// xattr recording git's executable bit, which lives on the tree *entry* that
/// named a blob rather than in the blob object — so it can't be recovered from
/// a bare hash when a placeholder is later fetched. Set (to `1`) whenever the
/// entry is an executable blob, on the placeholder and on the loaded file
/// alike. It is metadata only: a placeholder's *permissions* stay owner-only
/// with no exec bit, and the `+x` mode bit is added only once the file is
/// fetched (see [`write_file`]). Absent means a plain, non-executable blob.
const EXEC_XATTR: &str = "user.caos.exec";
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

    /// Is this object already stored? Cheap — no content crosses the wire.
    ///
    /// This is what makes `store` prune: a tree the store already holds is a
    /// tree whose whole subgraph it holds (git's closure invariant), so the
    /// walk stops there and nothing below it is read or sent.
    fn has_object(&self, hash: &str) -> Result<bool, String>;

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
            .with_header(caos_world::WORLD_HEADER, caos_world::WORLD)
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

    fn has_object(&self, hash: &str) -> Result<bool, String> {
        // HEAD, so a 37 MB binary costs a status line to ask about. The server
        // answers 200 or 404 with no body.
        let url = format!("{}/object/{hash}", self.base.trim_end_matches('/'));
        let response = minreq::head(&url)
            .with_header(caos_world::WORLD_HEADER, caos_world::WORLD)
            .send()
            .map_err(|e| format!("HEAD {url}: {e}"))?;
        match response.status_code {
            200..=299 => Ok(true),
            404 => Ok(false),
            code => Err(format!(
                "HEAD {url}: server returned {code} {}",
                response.reason_phrase
            )),
        }
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
    /// Its git directory, to reach the real index when staging into a
    /// throwaway one (`hash_dir`).
    git_dir: PathBuf,
    /// Canonical working-tree root used by every subprocess Git operation.
    /// Keeping it here prevents a transport from silently switching repos if
    /// the process working directory changes after discovery.
    work_dir: PathBuf,
}

impl GitTransport {
    /// Discover the working repo from the current directory. `caos-cli` must run
    /// inside a git working tree that has the server as its `caos` remote.
    pub fn from_cwd() -> Result<Self, String> {
        Self::discover(".")
    }

    /// Discover the working repository containing `path` and bind all future
    /// local Git commands to that worktree.
    pub fn discover(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let repo = gix::discover(path).map_err(|e| {
            format!("caos-cli must run inside a git working tree (none found): {e}")
        })?;
        let git_dir = repo.git_dir().to_path_buf();
        let work_dir = repo
            .workdir()
            .ok_or_else(|| {
                "caos-cli requires a working tree; bare repositories are unsupported".to_string()
            })?
            .canonicalize()
            .map_err(|e| format!("resolving the git working tree: {e}"))?;
        Ok(Self {
            repo,
            git_dir,
            work_dir,
        })
    }

    /// The worktree this transport and its subprocess Git commands operate on.
    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// Verify that the configured CAOS server accepts connections.
    ///
    /// The server deliberately returns 404 at its root, so any HTTP response
    /// proves reachability. This is a user-facing preflight for interactive
    /// clients: it fails before they take over the terminal and turns a later,
    /// low-level Git transport error into one concise diagnosis.
    pub fn ensure_server_reachable(&self) -> Result<(), String> {
        const TIMEOUT_SECS: u64 = 5;

        let url = self.server_url()?;
        minreq::get(url.trim_end_matches('/'))
            .with_timeout(TIMEOUT_SECS)
            .send()
            .map(|_| ())
            .map_err(|error| {
                format!(
                    "cannot reach the CAOS server at {url}: {error}\n\
                     check that it is running and that the `{CAOS_REMOTE}` git remote points to the right URL"
                )
            })
    }

    pub(crate) fn git_capture(
        &self,
        args: &[&str],
        index: Option<&Path>,
    ) -> Result<String, String> {
        git_capture_in(args, index, &self.work_dir)
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
        // lives there unreferenced). ONE object over the HTTP object API, not
        // `git fetch <hash>`, which would pull the object's whole CLOSURE.
        //
        // That distinction is the client's laziness, and it is worth real time.
        // A caller walking a tree (`checkout`) asks for the root, then only for
        // the children it doesn't already have — so an unchanged subtree stops
        // the walk dead. `git fetch` cannot do that: these results are raw
        // TREES, not commits, so git has nothing to negotiate with and the
        // server packs the entire graph every time. Measured on `run-tool
        // build` after a one-line worker edit, where 1 of 11 binaries actually
        // changes: 218 MB re-packed, re-indexed and re-inflated (~23s of a 38s
        // run) to deliver 12 MB of new bytes.
        //
        // `fetch_object`/`fetch_object_negotiated` stay for the COMMIT case
        // (chat turns), where a closure fetch is what you want and git has a
        // tip to negotiate against. Those write a PACK, which the cached `repo`
        // handle's odb will not see — so re-open before concluding the object
        // is absent, or every read after a chat's closure fetch would go back
        // over the wire one object at a time for objects already on disk.
        if let Ok(repo) = gix::open(&self.git_dir) {
            if let Ok(object) = repo.find_object(oid) {
                return Ok((object.kind.to_string(), object.data.clone()));
            }
        }
        let url = format!("{}/object/{hash}", self.server_url()?.trim_end_matches('/'));
        let serialized = http_get(&url)?;
        let (kind, content) = parse_object(&serialized)?;
        // Write it into the local repo so the next ask — and the next run — is
        // a local hit. put_object validates that the bytes hash to `hash`.
        let stored = self.put_object(kind, content)?;
        if stored != oid {
            return Err(format!("{url} returned an object hashing to {stored}"));
        }
        Ok((kind.to_string(), content.to_vec()))
    }

    fn has_object(&self, hash: &str) -> Result<bool, String> {
        // LOCAL presence, deliberately: this transport's `put_object` writes
        // locally too, and `ensure_pushed` moves the graph to the server in one
        // negotiated push. So "already stored" here means "already in the
        // working repo", which is exactly what lets `store` skip re-reading it.
        Ok(self.repo.find_object(parse_oid(hash)?).is_ok())
    }

    fn ensure_pushed(&self, hash: &str) -> Result<(), String> {
        // Content-addressed ref: clobber-free across clients, idempotent (a
        // re-push of the same content is a no-op), and it persists as the
        // negotiation base for the next push, so an edited tree ships only its
        // delta. The push carries the whole object graph reachable from `hash`.
        let refspec = format!("{hash}:refs/caos/req/{hash}");
        let push = || self.run_git(&["push", "--quiet", CAOS_REMOTE, &refspec]);

        // RETRIED, for the create race between clients pushing the same object.
        // They all read an advertisement without the ref, so they all plan a
        // CREATE, and every one that locks after the first dies with "cannot
        // lock ref …: reference already exists". `--force` does not help — the
        // create precondition comes from the advertised state, not from the
        // refspec.
        //
        // A retry usually succeeds because the winner has landed both the
        // objects and the ref (receive-pack updates the ref last), so the next
        // advertisement HAS it and the push becomes a no-op update — the ref
        // can only be at `hash`, the name is the content.
        //
        // MORE THAN ONE RETRY, because under load that is not guaranteed: two
        // losers can both re-read the advertisement before the winner's update
        // lands, both plan a create again, and one loses again. Measured with
        // six concurrent clients inside a loaded suite — a single retry left
        // five of six failing (tests/push-race).
        //
        // Retried on ANY error rather than by matching git's wording, which
        // varies by version: a few extra pushes on the failure path are cheaper
        // than a fragile string test, and a genuine failure just fails N times.
        let mut last = String::new();
        for attempt in 0..4 {
            match push() {
                Ok(_) => return Ok(()),
                Err(e) => last = e,
            }
            // Widening pause: the thing we are waiting for is another client's
            // ref update landing, which is brief but not instant.
            std::thread::sleep(std::time::Duration::from_millis(50 * (attempt + 1)));
        }
        // The LAST error, not the first: reporting attempt one's message hides
        // whatever actually defeated the retries, which is the only interesting
        // one (it cost a debugging session — the visible error said "reference
        // already exists" while the real failure was unknown).
        Err(format!("pushing {hash} to {CAOS_REMOTE}: {last}"))
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
        let out = self
            .git_capture(
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
        let out = self.git_capture(
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
        let out = self.git_capture(&["ls-files", "--", &abs.to_string_lossy()], None)?;
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
            let out = self.git_capture(&["rev-parse", "HEAD^{tree}"], None)?;
            return Ok((EntryKind::Tree.into(), parse_oid(out.trim())?));
        }

        let out = self.git_capture(&["ls-tree", "HEAD", "--", &abs.to_string_lossy()], None)?;
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
        let out = self.git_capture(&["hash-object", "-w", "--", &abs.to_string_lossy()], None)?;
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
            self.git_capture(&["add", "-u", "--", &abs.to_string_lossy()], Some(&tmp))?;
            let tree = if rel.as_os_str().is_empty() {
                self.git_capture(&["write-tree"], Some(&tmp))?
            } else {
                let prefix = format!("--prefix={}/", rel.to_string_lossy());
                self.git_capture(&["write-tree", &prefix], Some(&tmp))?
            };
            parse_oid(tree.trim())
        })();
        let _ = std::fs::remove_file(&tmp);
        Ok((EntryKind::Tree.into(), oid?))
    }
}

impl GitTransport {
    /// Run a network Git command in this transport's bound working tree.
    fn run_git(&self, args: &[&str]) -> Result<(), String> {
        self.git_capture(args, None).map(|_| ())
    }

    /// Fetch object `hash` (and its closure) from the `caos` remote into the
    /// local repo.
    ///
    /// `fetch.negotiationAlgorithm=noop` makes git send *no* "have" lines, so
    /// the negotiation is a single round. That's deliberate: the server's
    /// smart-HTTP delegate returns an empty body partway through a *multi-round*
    /// negotiation — which a client repo with real history (many refs/commits)
    /// triggers — and the fetch then dies with "the remote end hung up
    /// unexpectedly". The client and the caos server share no history anyway,
    /// so suppressing haves costs nothing here. `--no-write-fetch-head` also
    /// avoids the one shared worktree file otherwise touched by concurrent
    /// raw-object fetches; fetched objects still land in the shared object
    /// database.
    fn fetch_object(&self, hash: &str) -> Result<(), String> {
        self.run_git(&[
            "-c",
            "fetch.negotiationAlgorithm=noop",
            "fetch",
            "--quiet",
            "--no-write-fetch-head",
            CAOS_REMOTE,
            hash,
        ])
        .map_err(|e| format!("fetching {hash} from {CAOS_REMOTE}: {e}"))
    }

    /// Fetch object `hash` like [`Self::fetch_object`], but negotiate with `tip`
    /// (a commit the server is known to hold — e.g. just pushed) as the only
    /// negotiation tip, so the pack carries only what's new *since* `tip`
    /// instead of `hash`'s entire closure.
    ///
    /// Why not plain default negotiation: haves would walk every local ref and
    /// can go multi-round, which the smart-HTTP delegate has been seen to break
    /// on (see [`Self::fetch_object`]'s noop rationale). A single tip the server
    /// certainly has is ACKed in the first round, so the negotiation stays
    /// single-round *and* the pack stays minimal — without it, a turn fetch in
    /// a repo with real history re-downloads the whole workspace closure every
    /// turn (measured: ~10s of index-pack CPU per turn on a large repo).
    pub(crate) fn fetch_object_negotiated(&self, hash: &str, tip: &str) -> Result<(), String> {
        self.run_git(&[
            "-c",
            "fetch.negotiationAlgorithm=default",
            "fetch",
            "--quiet",
            "--no-write-fetch-head",
            "--negotiation-tip",
            tip,
            CAOS_REMOTE,
            hash,
        ])
        .map_err(|e| format!("fetching {hash} from {CAOS_REMOTE}: {e}"))
    }
}

/// Run `git` in `cwd` and return its stdout; error on failure. With `index` set,
/// `GIT_INDEX_FILE` points at a throwaway index (so `git add` / `write-tree` do
/// not touch the real one). The path-ingestion plumbing.
fn git_capture_in(args: &[&str], index: Option<&Path>, cwd: &Path) -> Result<String, String> {
    let mut command = std::process::Command::new("git");
    command.args(args).current_dir(cwd);
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
        .with_header(caos_world::WORLD_HEADER, caos_world::WORLD)
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
    // Git's executable bit was recorded as an xattr on the placeholder (it isn't
    // in the blob object). Read it before we replace the placeholder: now that
    // the file is being fetched, it becomes a real +x mode bit, and the xattr
    // rides along so a re-put / cas_entry reference still sees it as executable.
    // A top-level get-hash/put target has no placeholder, so exec stays false.
    let exec = xattr::get(target, EXEC_XATTR)
        .map(|v| v.is_some())
        .unwrap_or(false);
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
        // Fetched content: world-readable, writable by no one — plus git's exec
        // bit (mode and xattr) when the placeholder recorded it.
        let mode = if exec {
            xattr::set(tmp, EXEC_XATTR, b"1")
                .map_err(|e| format!("setting {EXEC_XATTR} on {}: {e}", tmp.display()))?;
            MODE_FETCHED_FILE | 0o111
        } else {
            MODE_FETCHED_FILE
        };
        set_mode(tmp, mode)
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
            // Git's executable bit isn't in the blob object, so record it as an
            // xattr — the placeholder's permissions stay owner-only; the exec
            // bit becomes a real mode bit only when the file is fetched.
            if entry.mode.kind() == EntryKind::BlobExecutable {
                xattr::set(&child, EXEC_XATTR, b"1")
                    .map_err(|e| format!("setting {EXEC_XATTR} on {}: {e}", child.display()))?;
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

/// The result kind recorded at `path`: its `KIND_XATTR` if present (a promise
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
    // A minted commit isn't stored via `send`, so assert here too: its message
    // (or tree/parent bytes) must not carry an injected secret.
    if !t.has_object(&hash_bytes("commit", &bytes)?.to_string())? {
        refuse_if_leaks(&bytes, "a commit")?;
    }
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
///
/// TWO PASSES, and the second one prunes. [`hash_path`] computes every id
/// locally, touching no network; [`send`] then walks top-down and stops at the
/// first object the store already has — which, by git's closure invariant,
/// means it has everything below it too. So re-storing a tree that moved by one
/// file sends one blob and the trees on its path, not the tree.
///
/// The old single pass posted every object as it hashed it. `caos-tools/build.sh`
/// puts a ~218 MB stack image on every run of which ~206 MB is byte-identical to
/// the last one (1 of 11 binaries actually changes on a one-line worker edit),
/// and it sent all of it, every time.
fn store(
    t: &dyn Transport,
    cas_real: Option<&Path>,
    path: &Path,
) -> Result<(gix::objs::tree::EntryMode, gix::ObjectId), String> {
    let hashed = hash_path(cas_real, path)?;
    send(t, &hashed)?;
    Ok((hashed.mode, hashed.oid))
}

/// A locally hashed source path: its git identity, plus what [`send`] needs to
/// store the object if the store turns out not to have it. File CONTENT is not
/// held — a blob is re-read from disk only when it is actually sent.
struct Hashed {
    mode: gix::objs::tree::EntryMode,
    oid: gix::ObjectId,
    body: Body,
}

enum Body {
    /// In the store by construction — a hash reused from the CAS, which is
    /// where it came from. Nothing to send, and nothing below it to walk.
    Stored,
    /// A regular file, re-read from this path to send.
    File(PathBuf),
    /// A symlink, whose blob *is* the link target.
    Link(Vec<u8>),
    /// A directory: its encoded tree bytes and its children.
    Dir(Vec<u8>, Vec<Hashed>),
}

/// The git tree entry for a real symlink at `path`: a blob holding the link
/// target, mode 120000. Used both for a genuine symlink outside the CAS and to
/// preserve a CAS node that is itself a symlink (rather than reusing its
/// dereferenced target's hash).
fn link_entry(path: &Path) -> Result<Hashed, String> {
    let link = std::fs::read_link(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let target = link.as_os_str().as_bytes().to_vec();
    let oid = hash_bytes("blob", &target)?;
    Ok(Hashed {
        mode: gix::objs::tree::EntryKind::Link.into(),
        oid,
        body: Body::Link(target),
    })
}

/// Resolve a staging symlink to the CAS node it names, WITHOUT dereferencing
/// that node itself. Returns the node's real path when it lands inside
/// `cas_real`, else `None` — a genuine symlink pointing elsewhere, which the
/// caller records as an ordinary git symlink.
///
/// Workers stage a result by symlinking already-fetched `/cas/...` entries into
/// a scratch tree (outside the CAS) and `caos put`ting it — that is how an
/// agent's write/edit keeps every untouched sibling. The node such a link names
/// may itself be a git symlink (materialized as a real symlink, e.g. a
/// `CLAUDE.md -> AGENTS.md`); a plain `canonicalize()` of the staging link would
/// resolve THROUGH it to its target and reuse that file's blob hash, flattening
/// the symlink into a regular copy. So resolve the link one hop and canonicalize
/// only the DIRECTORY of the node it points at, then re-attach the node's name,
/// leaving the node's own symlink-ness for the caller to preserve.
fn cas_node(link: &Path, cas_real: &Path) -> Option<PathBuf> {
    let hop = std::fs::read_link(link).ok()?;
    let hop = if hop.is_absolute() {
        hop
    } else {
        link.parent()?.join(hop)
    };
    let dir = hop.parent()?;
    let name = hop.file_name()?;
    if let Ok(dir) = dir.canonicalize() {
        let node = dir.join(name);
        if node != cas_real && node.starts_with(cas_real) {
            return Some(node);
        }
    }
    None
}

/// Hash `path` into git objects without storing anything. Same shape rules as
/// [`store`]: symlinks into the CAS reuse their recorded hash, other symlinks
/// are blobs holding the link target, directories are trees.
fn hash_path(cas_real: Option<&Path>, path: &Path) -> Result<Hashed, String> {
    use gix::objs::tree::EntryKind;

    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let ft = meta.file_type();

    if ft.is_symlink() {
        if let Some(cas_real) = cas_real {
            if let Some(node) = cas_node(path, cas_real) {
                let node = node.as_path();
                // A CAS node that is itself a git symlink must be recorded AS a
                // symlink carrying its own target, not dereferenced onto its
                // target's content — which is what fully canonicalizing the
                // staging link would do, silently rewriting a symlink into a
                // regular copy (e.g. a `CLAUDE.md -> AGENTS.md` staged untouched
                // by an agent turn).
                if node.is_symlink() {
                    return link_entry(node);
                }
                let (mode, oid) = cas_entry(node)?;
                return Ok(Hashed {
                    mode,
                    oid,
                    body: Body::Stored,
                });
            }
        }
        return link_entry(path);
    }

    if ft.is_dir() {
        let mut entries = Vec::new();
        let mut children = Vec::new();
        for dirent in std::fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))? {
            let dirent = dirent.map_err(|e| format!("{}: {e}", path.display()))?;
            let child = hash_path(cas_real, &dirent.path())?;
            entries.push(gix::objs::tree::Entry {
                mode: child.mode,
                filename: dirent.file_name().into_vec().into(),
                oid: child.oid,
            });
            children.push(child);
        }
        // Git requires tree entries in a specific order; Entry's Ord implements it.
        entries.sort();
        let mut buf = Vec::new();
        gix::objs::Tree { entries }
            .write_to(&mut buf)
            .map_err(|e| format!("encoding tree for {}: {e}", path.display()))?;
        let oid = hash_bytes("tree", &buf)?;
        return Ok(Hashed {
            mode: EntryKind::Tree.into(),
            oid,
            body: Body::Dir(buf, children),
        });
    }

    if ft.is_file() {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let oid = hash_bytes("blob", &data)?;
        let kind = if meta.permissions().mode() & 0o111 != 0 {
            EntryKind::BlobExecutable
        } else {
            EntryKind::Blob
        };
        return Ok(Hashed {
            mode: kind.into(),
            oid,
            body: Body::File(path.to_path_buf()),
        });
    }

    Err(format!("unsupported file type: {}", path.display()))
}

/// Store everything under `h` that the transport doesn't already have. A hit
/// prunes the whole subtree: caos only ever writes a tree AFTER the objects it
/// names, and the server's repo has GC disabled, so a stored tree's descendants
/// are stored too.
fn send(t: &dyn Transport, h: &Hashed) -> Result<(), String> {
    if matches!(h.body, Body::Stored) || t.has_object(&h.oid.to_string())? {
        return Ok(());
    }
    // The output-leak assertion (design/secrets.md): scan each NEW blob for any
    // secret injected into this run BEFORE it is published. We reach here only
    // for objects the store lacks, so — by git's closure invariant — only for
    // objects this run is introducing; an output that dedups to something
    // already stored can't be a new leak. On a hit the whole `put` fails, so
    // the offending object is never posted. (Off-worker there is no `/secret`,
    // so this is a no-op for the host CLI.)
    let stored = match &h.body {
        Body::Stored => unreachable!("filtered above"),
        Body::Link(target) => {
            refuse_if_leaks(target, "a symlink target")?;
            t.put_object("blob", target)?
        }
        Body::File(path) => {
            let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            refuse_if_leaks(&data, &path.display().to_string())?;
            t.put_object("blob", &data)?
        }
        Body::Dir(encoded, children) => {
            for child in children {
                send(t, child)?;
            }
            t.put_object("tree", encoded)?
        }
    };
    if stored != h.oid {
        return Err(format!(
            "stored object hashes to {stored}, not the {} computed locally",
            h.oid
        ));
    }
    Ok(())
}

/// The in-container directory the runner drops this run's granted secrets into,
/// one file per secret (design/secrets.md). Shared with the container runner
/// (`bin/caos.rs`), which writes it.
pub const SECRET_DIR: &str = "/secret";

/// The raw values of every secret injected into this run, read once from
/// [`SECRET_DIR`]. Empty off-worker (no such dir) — so the leak scan costs
/// nothing for the host CLI. Empty values are dropped (nothing to match).
fn injected_secret_values() -> &'static [Vec<u8>] {
    static VALUES: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    VALUES.get_or_init(|| {
        let mut values = Vec::new();
        if let Ok(entries) = std::fs::read_dir(SECRET_DIR) {
            for entry in entries.flatten() {
                if let Ok(bytes) = std::fs::read(entry.path()) {
                    if !bytes.is_empty() {
                        values.push(bytes);
                    }
                }
            }
        }
        values
    })
}

/// Fail if `data` contains any injected secret's raw bytes — the hard output
/// assertion. `what` names the object for the error, and the error deliberately
/// never quotes the value. Raw-byte only: a secret the worker base64'd or
/// otherwise transformed slips through (design/secrets.md — this catches bugs,
/// a token swept into an error file or a stray credential, not a determined
/// exfiltrator, which is inside the trust boundary anyway).
fn refuse_if_leaks(data: &[u8], what: &str) -> Result<(), String> {
    for secret in injected_secret_values() {
        if contains_subslice(data, secret) {
            return Err(format!(
                "refusing to store {what}: it contains an injected secret value \
                 (design/secrets.md: outputs must not carry secrets)"
            ));
        }
    }
    Ok(())
}

/// Does `haystack` contain `needle` as a contiguous byte subslice?
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The git object id `kind`/`data` would have, computed locally — no store.
fn hash_bytes(kind: &str, data: &[u8]) -> Result<gix::ObjectId, String> {
    let object_kind = gix::object::Kind::from_bytes(kind.as_bytes())
        .map_err(|e| format!("unknown object kind {kind}: {e}"))?;
    gix::objs::compute_hash(gix::hash::Kind::Sha1, object_kind, data)
        .map_err(|e| format!("hashing a {kind}: {e}"))
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
    } else if is_executable(canon) {
        // The exec bit `write_tree`/`write_file` preserved on this CAS node —
        // so an executable blob round-trips as one, not a plain blob.
        EntryKind::BlobExecutable
    } else {
        EntryKind::Blob
    };
    Ok((kind.into(), parse_oid(&read_hash(canon)?)?))
}

/// Whether the CAS node at `path` is an executable blob — recorded by
/// [`write_tree`]/[`write_file`] as the [`EXEC_XATTR`], not as a mode bit (a
/// placeholder's permissions carry no exec bit), so this reads the xattr.
fn is_executable(path: &Path) -> bool {
    xattr::get(path, EXEC_XATTR)
        .map(|v| v.is_some())
        .unwrap_or(false)
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
/// Each `--name[:type]=value` becomes a tree entry `name` (see [`parse_arg`]):
/// * `--name=value` — a literal, stored verbatim as a blob;
/// * `--name:@=path` inside the CAS — references the object that path was
///   materialized from (its recorded hash). Only when `cas` is `Some` (the
///   worker); the CLI passes `None`, so every path is a host path;
/// * `--name:@=path` elsewhere — a host path, ingested via the transport (the git
///   transport ingests it from the working repo, and only if git tracks it — see
///   [`GitTransport::ingest_path`]); a worker has no host filesystem, so this is
///   an error there;
/// * `--name:commit=value` — a commit, passed unpeeled as a gitlink entry (see
///   [`resolve_commit_arg`]);
/// * `--name:tree=hash` — a tree the server already holds (an earlier run's
///   result), referenced directly by hash.
fn build_arg_entries(
    t: &dyn Transport,
    cas: Option<&Path>,
    kvs: &[String],
) -> Result<Vec<gix::objs::tree::Entry>, String> {
    use gix::objs::tree::{Entry, EntryKind};

    let mut entries = Vec::new();
    for kv in kvs {
        let (name, ty, value) = parse_arg(kv)?;

        let (mode, oid) = match ty {
            // `--name=value` — store the literal verbatim as a blob.
            ArgType::Literal => (
                EntryKind::Blob.into(),
                post_object(t, "blob", value.as_bytes())?,
            ),
            // `--name:@=path` under the CAS — reference whatever it was made from.
            ArgType::Path if cas.is_some_and(|c| Path::new(value).starts_with(c)) => {
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
                cas_entry(&canon)?
            }
            // `--name:@=path` elsewhere — ingest a host path (git transport only;
            // the worker has no host filesystem, so it errors clearly).
            ArgType::Path => t.ingest_path(value)?.ok_or_else(|| {
                format!("`{name}`: {value:?} is a host path, but this client only reads /cas paths")
            })?,
            // `--name:commit=value` — a commit, unpeeled, as a gitlink entry.
            ArgType::Commit => (
                EntryKind::Commit.into(),
                resolve_commit_arg(t, cas, value).map_err(|e| format!("`{name}`: {e}"))?,
            ),
            // `--name:tree=hash` — a tree the server already holds (an earlier
            // result), referenced by hash. Verified server-side to be a tree so
            // a typo fails here, not as a bad materialization in the worker.
            ArgType::Tree => {
                let (kind, _) = t
                    .get_object(value)
                    .map_err(|e| format!("`{name}`: tree {value}: {e}"))?;
                if kind != "tree" {
                    return Err(format!("`{name}`: {value} is a {kind}, not a tree"));
                }
                (EntryKind::Tree.into(), parse_oid(value)?)
            }
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

/// The **type tag** of a `--name[:type]=value` argument — the operator's
/// explicit choice of how the value is read (never sniffed from the value's
/// shape, so a value may start with anything, no escaping). Bare `=` is a
/// literal; `:@=` a path; `:commit=` a commit; `:tree=` a tree hash.
///
/// This is the ONE arg-type vocabulary, shared by the CLI/worker arg builder
/// ([`build_arg_entries`]), the map-then image args, and the `.caos-expr`
/// evaluator (`resolve_expr_args`), so all of them accept exactly the same types
/// and emit the same errors. A resolver may still support only a subset — the
/// evaluator resolves literals and paths today — but they all *parse* through
/// [`parse_arg`]. The grammar is extensible: a new type adds a variant here, a
/// case in [`parse_arg`], and an arm in each resolver.
pub(crate) enum ArgType {
    /// `--name=value` — the value verbatim, stored as a blob.
    Literal,
    /// `--name:@=path` — the value names a path to resolve/ingest (a host path
    /// on the CLI, a `/cas` path in a worker, a tree path in the evaluator).
    Path,
    /// `--name:commit=value` — the value names a *commit*, passed **unpeeled**
    /// as a gitlink entry: a commit hash, a `/cas` path recorded as a commit
    /// (worker), or a revspec like `HEAD` (CLI). The explicit opt-in exists
    /// because the default forms peel commits to trees (which image refs rely
    /// on); see [`resolve_commit_arg`].
    Commit,
    /// `--name:tree=hash` — the value is the hash of a tree the *server*
    /// already holds (typically an earlier run's result), referenced directly
    /// as a tree entry with no content round-trip. This is how results compose
    /// into new requests: e.g. a workspace-build job's `bin` tree feeding a
    /// downstream job as `--bins:tree=<hash>`.
    Tree,
}

/// Split a `--name[:type]=value` argument into its name, [`ArgType`] and raw
/// value, validating that the name is a single path component (it becomes a
/// tree-entry filename). Shared by every arg resolver so the type vocabulary and
/// its errors are defined exactly once.
pub(crate) fn parse_arg(kv: &str) -> Result<(&str, ArgType, &str), String> {
    let body = kv
        .strip_prefix("--")
        .ok_or_else(|| format!("argument must look like --name=value, got: {kv}"))?;
    let (key, value) = body
        .split_once('=')
        .ok_or_else(|| format!("argument must look like --name[:type]=value, got: {kv}"))?;
    // The key is `name` (literal) or `name:type` (typed); the type sits before `=`.
    let (name, ty) = match key.split_once(':') {
        None => (key, ArgType::Literal),
        Some((name, "@")) => (name, ArgType::Path),
        Some((name, "commit")) => (name, ArgType::Commit),
        Some((name, "tree")) => (name, ArgType::Tree),
        Some((_, ty)) => {
            return Err(format!(
                "unknown argument type {ty:?} in {kv:?}; use --name=value (literal), \
                 --name:@=value (path), --name:commit=value (commit), or \
                 --name:tree=hash (a tree the server already holds)"
            ))
        }
    };
    if name.is_empty() || name.contains('/') {
        return Err(format!(
            "argument name must be a single path component, got: {name:?}"
        ));
    }
    Ok((name, ty, value))
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
    trace: Option<(&str, &mut (dyn Write + Send))>,
    kvs: &[String],
    store: &[ClientSecret],
) -> Result<(String, String), String> {
    let arg_tree = prepare_request(t, image, cas, kvs, store)?;
    let header = secret_store_header(store);
    // Trigger compute; the server runs the container and returns the result's
    // "<type> <hash>" (and, for a top-level run, pins refs/caos/res/<argTreeHash>
    // at it).
    let server = t.server_url()?;
    match trace {
        Some((id, output)) => request_compute_streamed(&server, &arg_tree, id, output, &header),
        None => request_compute(&server, &arg_tree, &header),
    }
}

/// Everything in [`run_request`] up to (and including) getting the ArgTree onto
/// the server, returning its hash (the arg-tree hash — the request id). Split out
/// so a caller can trigger the blocking compute itself — `chat` runs
/// [`request_compute`] on its own thread (it needs only the arg-tree hash and the
/// server URL, both plain strings) while it watches the turn's progress ref from
/// the main one.
fn prepare_request(
    t: &dyn Transport,
    image: &str,
    cas: Option<&Path>,
    kvs: &[String],
    store: &[ClientSecret],
) -> Result<String, String> {
    // Build the call's args (paths resolve per `cas`), then hand them to the
    // shared assembler, which folds in the image, salt, std and secret-hash.
    let call = build_arg_entries(t, cas, kvs)?;
    assemble_arg_tree(t, image, call, store)
}

/// Assemble a runnable ArgTree from a base `image` ref and the caller's already
/// resolved `call` args, folding in the reserved `image`/`salt`/`std` entries,
/// storing it, and getting it onto the server. Returns the ArgTree hash (the
/// request id and cache key). Shared by [`prepare_request`] (which resolves
/// `call` from kvs) and the `.caos-expr` evaluator (which resolves `call`
/// against a git tree).
fn assemble_arg_tree(
    t: &dyn Transport,
    image: &str,
    call: Vec<gix::objs::tree::Entry>,
    store: &[ClientSecret],
) -> Result<String, String> {
    // Expand any curry layers: pull the underlying image out and collect the args
    // bound into it. The image is folded into the args tree below, so the server
    // only ever sees a plain args tree.
    let (image, bound) = unwrap_curry(t, image)?;

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
    let mut arg_entries = merge_entries(merge_entries(bound, call), vec![image_entry]);

    // The cache-busting salt (empty by default) rides *in* the args tree under the
    // reserved `salt` entry, exactly like `image` — per SPEC an ArgTree is a git
    // tree of named args including `salt`, so the salt belongs there rather than
    // as a sibling of `args` in the request. Since the args tree is the cache key,
    // a salted run is simply a different args tree; it needs no keying of its own.
    // Absent (the common case) it adds nothing, so an unsalted run's args tree —
    // and request — is unchanged. Threaded into sub-runs via CAOS_SALT.
    let salt = run_salt();
    if !salt.is_empty() {
        arg_entries = merge_entries(arg_entries, vec![salt_arg_entry(t, &salt)?]);
    }

    // Cache-isolation tag (design/secrets.md): fold in `secret-hash` when this
    // run is granted a secret — matched against the entries built so far, so two
    // callers with different secrets don't share a cache entry, while a
    // secret-free run stays globally shared. Byte-identical to what the server
    // folds into an equivalent sub-run ArgTree.
    let base: std::collections::BTreeMap<String, String> = arg_entries
        .iter()
        .map(|e| {
            (
                String::from_utf8_lossy(e.filename.as_ref()).into_owned(),
                e.oid.to_string(),
            )
        })
        .collect();
    if let Some(hash) = client_secret_hash(store, &base)? {
        let oid = post_object(t, "blob", hash.as_bytes())?;
        let entry = gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: caos_world::SECRET_HASH_ARG.as_bytes().to_vec().into(),
            oid,
        };
        arg_entries = merge_entries(arg_entries, vec![entry]);
    }

    // The request object IS the args tree — the ArgTree — so its hash *is* the
    // request id and the server's cache key, with nothing keyed alongside it
    // (image and salt are entries within). Get it onto the server — a
    // no-op POST-as-you-go for the HTTP transport, a push for the git one. The
    // push carries the whole graph reachable from the tree, which includes any
    // embedded git-image tree, so the image lands on the server without a
    // separate push.
    let arg_tree = post_tree(t, arg_entries)?;
    t.ensure_pushed(&arg_tree.to_string())?;
    Ok(arg_tree.to_string())
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
    record_continuation(t, "map-then", input, kvs, &["map", "then"], &[], |given| {
        if given.is_empty() {
            return Err("`map-then` needs --map and/or --then".to_string());
        }
        Ok(())
    })
}

/// `run-then <in> -- --run=<image> [--then=<image>] [--catch]` — the
/// single-valued [`caos_map_then`]: record a continuation `{in, run, then?,
/// catch?}` as this worker's result at `/cas/out` and exit. The server runs
/// `run(--in=<in>)` once, yielding R; with `--then` the request's result is
/// `then(--in=<in>, --result=<R>)` (symmetric with map-then's
/// `--in`/`--children`), else R itself — so `run` with no `then` is a plain tail
/// call to `run`. `--run` is required (a bare tail call to one image); `--map`
/// doesn't belong here — `map` and `run` are mutually exclusive, which this
/// surface enforces client-side. Image refs resolve exactly as in `map-then`.
///
/// `--catch` (a bare flag) makes a FAILING `run` a value instead of an error:
/// the `then` is called with `--error=<blob of the failure text>` in place of
/// `--result`, and the request succeeds. It needs `--then` — there is nowhere
/// else for the error to go — and the enclosing request is then left uncached,
/// so a retry really retries. Reach for it when the caller's job is to react to
/// the failure rather than propagate it: the agent loop wants a failed tool to
/// come back as an `is_error` tool_result, not to kill the turn.
pub fn caos_run_then(t: &dyn Transport, input: &str, kvs: &[String]) -> Result<(), String> {
    record_continuation(
        t,
        "run-then",
        input,
        kvs,
        &["run", "then"],
        &["catch"],
        |given| {
            if !given.contains(&"run") {
                return Err("`run-then` needs --run (with an optional --then)".to_string());
            }
            if given.contains(&"catch") && !given.contains(&"then") {
                return Err(
                    "`run-then --catch` needs --then: the error has to be delivered somewhere"
                        .to_string(),
                );
            }
            Ok(())
        },
    )
}

/// Shared body of [`caos_map_then`] / [`caos_run_then`]: record a continuation
/// `{in, <images>}` over `input` as this worker's result at `/cas/out` (a
/// `promise` placeholder the server resolves once the job is posted). `allowed`
/// names the image-valued entries this verb accepts — the surface split is the
/// client-side mutual exclusion of `map` and `run` — `markers` names its bare
/// flags (recorded as one-byte blobs; the interpreter reads only their
/// presence), and `check` validates the set actually given, before anything is
/// sealed.
fn record_continuation(
    t: &dyn Transport,
    verb: &str,
    input: &str,
    kvs: &[String],
    allowed: &[&'static str],
    markers: &[&'static str],
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
        // Markers are bare flags, matched BEFORE parse_kv — which requires a
        // `=value` and would reject them. Presence is the whole signal, so the
        // recorded blob's content is arbitrary; the interpreter never reads it.
        if let Some(&name) = markers.iter().find(|&&m| kv.strip_prefix("--") == Some(m)) {
            if given.contains(&name) {
                return Err(format!("--{name} given twice"));
            }
            entries.push(Entry {
                mode: EntryKind::Blob.into(),
                filename: name.as_bytes().to_vec().into(),
                oid: post_object(t, "blob", b"1")?,
            });
            given.push(name);
            continue;
        }
        let (name, ty, value) = parse_arg(kv)?;
        let Some(&name) = allowed.iter().find(|&&a| a == name) else {
            let mut flags = allowed
                .iter()
                .map(|a| format!("--{a}"))
                .collect::<Vec<_>>()
                .join(" and ");
            if !markers.is_empty() {
                let m = markers
                    .iter()
                    .map(|a| format!("--{a}"))
                    .collect::<Vec<_>>()
                    .join(" and ");
                flags = format!("{flags} (each an image ref) and the flag {m}");
                return Err(format!("`{verb}` takes only {flags}, got --{name}"));
            }
            return Err(format!(
                "`{verb}` takes only {flags} (each an image ref), got --{name}"
            ));
        };
        if given.contains(&name) {
            return Err(format!("--{name} given twice"));
        }
        // Literal, path or tree-hash all name an image ref (a /cas path, a bare
        // hash, docker://, or a git image / curry node); resolve_run_image
        // handles the shapes. A commit is not an image.
        let image = match ty {
            ArgType::Commit => {
                return Err(format!("--{name} names an image, not a commit"));
            }
            _ => value,
        };
        let resolved = resolve_run_image(t, &cas, image)?;
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

/// `run <image | dir> [output] -- [--name=value | --name:@=path ...]`
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
/// `<image>` is a `docker://` ref, a bare hash, or a host DIRECTORY — evaluated
/// if it carries a `.caos-expr` (see [`resolve_cli_image`]).
pub fn cli_run(
    t: &dyn Transport,
    image: &str,
    output: Option<&str>,
    trace: Option<(&str, &mut (dyn Write + Send))>,
    kvs: &[String],
) -> Result<(), String> {
    if trace.as_ref().is_some_and(|(id, _)| !valid_trace_id(id)) {
        return Err("trace id must be 1-128 ASCII letters, digits, '-' or '_'".to_string());
    }
    let image = resolve_cli_image(t, image)?;
    // Build the ephemeral secrets store from the caller's `.caos-secrets`
    // (design/secrets.md), resolving each reader here — where eval-path is
    // available — so the server never evals. Empty when there's no store.
    let store = build_secret_store(t)?;
    let (kind, result) = run_request(t, &image, None, trace, kvs, &store)?;

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
    // tree, every descendant — so it's readable and editable on the host. With
    // the output going to files, stdout carries the result's identity —
    // "<kind> <hash>" — so a script can thread it onward (e.g. as a
    // `--name:tree=` arg to a later run).
    println!("{kind} {result}");
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

/// Build the args tree's reserved `salt` entry: the cache-busting salt as a plain
/// blob. The counterpart of [`image_arg_entry`] for the other
/// reserved ArgTree member; merged in only when the salt is non-empty.
fn salt_arg_entry(t: &dyn Transport, salt: &str) -> Result<gix::objs::tree::Entry, String> {
    use gix::objs::tree::{Entry, EntryKind};
    Ok(Entry {
        mode: EntryKind::Blob.into(),
        filename: b"salt".to_vec().into(),
        oid: post_object(t, "blob", salt.as_bytes())?,
    })
}

/// The cache-busting salt for this run (see [`SALT_ENV`]): read from `CAOS_SALT`,
/// empty if unset. Read at the top of a run (the CLI); the server threads it
/// into each worker and every promise sub-run — so a whole run tree shares one.
fn run_salt() -> String {
    std::env::var(SALT_ENV).unwrap_or_default()
}

/// Resolve a git ref to its tree hash, read from the local
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
fn resolve_run_image(t: &dyn Transport, cas: &Path, image: &str) -> Result<String, String> {
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
        // A `docker://` image has no git object, so it rides as a *blob naming
        // the ref* (see `read_request`); a file holding such a ref resolves
        // to the ref itself — its recorded blob hash names an object no engine
        // could run. Fetch the blob rather than reading the file: a CAS entry
        // is a content-less placeholder until someone `get`s it.
        if canon.is_file() {
            let hash = read_hash(&canon)?;
            if let Ok(content) = fetch_blob_string(t, &hash) {
                if content.starts_with(DOCKER_SCHEME) {
                    return Ok(content);
                }
            }
            return Ok(hash);
        }
        return read_hash(&canon);
    }
    Err(format!(
        "image must be a path under {} (a git image), a git hash, or \
         {DOCKER_SCHEME}<ref>, got: {image}",
        cas.display()
    ))
}

/// Resolve a `caos-cli run` image argument to something the server can run: a
/// DIRECTORY is ingested (and evaluated, if it is evaluable), and every other
/// form (`docker://…`, a bare hash) is left untouched.
///
/// A directory is the only name a caller needs, because a tree says how it is
/// built. There is deliberately no name-to-image lookup here: the CLI resolves
/// a dependency by DESCENT through the tree it was handed (`DEEP-DEPS/<name>`),
/// which is what makes a caller's dependencies its own declared edges rather
/// than whatever an ambient library happens to hold.
pub fn resolve_cli_image(t: &dyn Transport, image: &str) -> Result<String, String> {
    // A directory is an image tree to ingest: ingest it exactly like a
    // `--name:@=path` arg (git-tracked paths only) and hand the server its tree
    // hash. A flake dir is NOT special-cased here or on the server — it carries
    // a `.caos-expr` naming its builder, and the evaluation below turns it into
    // an image, so what the server receives is already one (design/caos-expr.md).
    if Path::new(image).is_dir() {
        let (_, oid) = t
            .ingest_path(image)?
            .ok_or_else(|| format!("this transport cannot ingest the image dir {image}"))?;
        // ...unless the directory is EVALUABLE. A tree carrying a `.caos-expr`
        // says how it is built, so resolving it means evaluating it — the same
        // rule `resolve_expr_image` applies to a path named inside an
        // expression. This is what lets a caller name a dependency by its
        // deep-deps mount (`run DEEP-DEPS/rgrep`) — a path it holds, not a name
        // it looks up; a tree with no `.caos-expr` (a plain flake dir, a
        // git-docker image) evaluates to itself and nothing changes.
        return eval::eval_tree(t, &oid.to_string());
    }
    Ok(image.to_string())
}

/// `curry <arg tree> [--unbind=<name> …] -- [--name=value ...]` — bind arguments
/// to `<arg tree>`, printing a ref (a git hash) to the new arg tree that includes
/// the new args. The ref can be `run` — which supplies the rest of the args —
/// or `curry`'d again, exactly like any other arg tree; the binding is partial
/// application, not a rebuilt container image. This is the *worker* form: path
/// args resolve against `/cas`. (The CLI's is [`cli_curry`].)
///
/// Currying is an ArgTree → ArgTree operation. `arg_tree` may be given in any of
/// its equivalent forms — a curry node, a flat args tree (e.g. `own_args_tree`),
/// or a bare image (the *simplest* ArgTree, image and nothing else) — because
/// `unwrap_curry` normalizes whichever it is into the `(base image, bound
/// args)` pair an ArgTree decomposes to. So no caller has to wrap a bare image
/// first; that wrapping is exactly the empty-bound-args case here.
///
/// The result is a small CAS tree: a `base` blob (the image), an `args` subtree
/// (the bound args, in `build_arg_entries` shape), and a [`CURRY_MARKER`] blob.
/// Currying flattens: if `arg_tree` is itself curried, its bindings are folded in
/// and `base` stays a plain (docker/git) image, so the result is canonical
/// (`curry (curry img a) b` == `curry img a b`) — and STRICT: rebinding an
/// already-bound name is refused, not overridden, unless it is first `--unbind`ed
/// (see `curry_object`).
pub fn caos_curry(t: &dyn Transport, arg_tree: &str, rest: &[String]) -> Result<(), String> {
    let cas = cas_dir();
    let (unbind, kvs) = split_curry_args(rest)?;
    let arg_tree = resolve_run_image(t, &cas, arg_tree)?;
    println!("{}", curry_object(t, &arg_tree, Some(&cas), &unbind, kvs)?);
    Ok(())
}

/// `curry <arg tree> [--unbind=<name> …] -- [--name=value ...]` — the *CLI* form
/// of [`caos_curry`]: `<arg tree>` may be a directory to ingest and evaluate,
/// path args are host paths to ingest, and the curried
/// arg tree is pushed so a later `run` can use the printed ref directly.
pub fn cli_curry(t: &dyn Transport, arg_tree: &str, rest: &[String]) -> Result<(), String> {
    let (unbind, kvs) = split_curry_args(rest)?;
    let arg_tree = resolve_cli_image(t, arg_tree)?;
    let curried = curry_object(t, &arg_tree, None, &unbind, kvs)?;
    t.ensure_pushed(&curried.to_string())?;
    if is_hex_hash(&arg_tree) {
        t.ensure_pushed(&arg_tree)?;
    }
    println!("{curried}");
    Ok(())
}

/// Split a `curry`'s tail — `[--unbind=<name> …] -- [--name=value …]` — into the
/// unbind names (before `--`) and the bind kvs (after it). The `--` is required,
/// so the two regions never blur; only `--unbind=<name>` is accepted before it,
/// which cannot collide with a bound arg (a different region entirely).
fn split_curry_args(rest: &[String]) -> Result<(Vec<&str>, &[String]), String> {
    let sep = rest
        .iter()
        .position(|a| a == "--")
        .ok_or("curry needs `--` before its args (e.g. `curry <arg tree> -- --k=v`)")?;
    let (flags, kvs) = (&rest[..sep], &rest[sep + 1..]);
    let unbind = flags
        .iter()
        .map(|f| {
            f.strip_prefix("--unbind=").ok_or_else(|| {
                format!(
                    "curry: unexpected {f:?} before `--`; only --unbind=<name> is allowed there"
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((unbind, kvs))
}

/// Build (and store) a curry node from the ArgTree `arg_tree` plus `unbind`/`kvs`:
/// the shared body of [`caos_curry`] / [`cli_curry`]. [`unwrap_curry`] decomposes
/// `arg_tree` (whatever form it's in) into its `base` image and already-`bound`
/// args; we then drop the `unbind` names, refuse any rebind, and add `kvs`. `cas`
/// decides how path args resolve, exactly as in [`run_request`].
fn curry_object(
    t: &dyn Transport,
    arg_tree: &str,
    cas: Option<&Path>,
    unbind: &[&str],
    kvs: &[String],
) -> Result<gix::ObjectId, String> {
    let new = build_arg_entries(t, cas, kvs)?;
    curry_from_entries(t, arg_tree, unbind, new)
}

/// The body of [`curry_object`] once the new args are resolved into `new`
/// entries: decompose `arg_tree` into `(base, bound)`, drop the `unbind` names,
/// refuse any rebind, add `new`, and store the curry node. Shared with the
/// `.caos-expr` evaluator, which resolves its `new` entries against a git tree
/// rather than from kvs.
fn curry_from_entries(
    t: &dyn Transport,
    arg_tree: &str,
    unbind: &[&str],
    new: Vec<gix::objs::tree::Entry>,
) -> Result<gix::ObjectId, String> {
    use gix::objs::tree::{Entry, EntryKind};

    let (base, mut bound) = unwrap_curry(t, arg_tree)?;

    // UNBIND first: drop the named args so they can be rebound. Currying is
    // otherwise strict (below), so carrying a whole ArgTree forward and changing
    // a few of its args — the self-recurry case — needs an explicit release. An
    // unbind of a name that isn't bound is a mistake (a typo, or a wrong
    // assumption about the ArgTree's shape), so it's an error, not a no-op.
    for name in unbind {
        let before = bound.len();
        bound.retain(|e| entry_name(e) != name.as_bytes());
        if bound.len() == before {
            return Err(format!(
                "curry: --unbind={name} but {name:?} is not bound in {arg_tree}"
            ));
        }
    }

    // REFUSE to rebind a name that's already bound: a colliding curry is
    // almost always the reserved-name class of bug (a caller arg landing on
    // `worker1`), and silent override turns it into a distant, cryptic
    // failure. Call-time args still override curry bindings at run — only
    // curry-over-curry is strict. Unbind (above) is the deliberate release.
    for e in &new {
        if bound.iter().any(|b| b.filename == e.filename) {
            return Err(format!(
                "curry: arg {:?} is already bound in {arg_tree}; rename one of them, \
                 or --unbind it first (curry refuses to rebind — run-time args \
                 may still override)",
                String::from_utf8_lossy(&e.filename)
            ));
        }
    }
    let args = merge_entries(bound, new);
    let args_tree = post_tree(t, args)?;

    let entries = vec![
        Entry {
            mode: EntryKind::Blob.into(),
            filename: b"base".to_vec().into(),
            oid: post_object(t, "blob", base.as_bytes())?,
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
        if let Some((inner_image, inner_args)) = curry_node(t, &image)? {
            // `bound` holds outer layers, which win over this deeper one.
            bound = merge_entries(inner_args, bound);
            image = inner_image;
            continue;
        }
        // The OTHER form of an ArgTree: a flat args tree `{image, …args}` (no
        // curry marker), which is what the server materializes at `/cas/args`
        // and `own_args_tree` names. Its `image` entry is the base; its other
        // entries are bound args. Recognizing it lets `curry` carry a whole
        // ArgTree forward — bind/unbind onto it — not just re-bind onto a bare
        // base image.
        if let Some((inner_image, inner_args)) = args_tree_node(t, &image)? {
            bound = merge_entries(inner_args, bound);
            image = inner_image;
            continue;
        }
        break; // a plain image (git-docker/flake), not an ArgTree
    }
    Ok((image, bound))
}

/// If `hash` names a flat **args tree** — a tree carrying the reserved `image`
/// entry but no [`CURRY_MARKER`] — return its base image ref (from the `image`
/// entry: a git image's tree oid, or a `docker://` blob's contents) and its
/// remaining entries as bound args. This is the shape the server materializes at
/// `/cas/args` (hence what `own_args_tree` names); `None` for a curry node, a
/// plain image, or any tree without an `image` entry.
fn args_tree_node(
    t: &dyn Transport,
    hash: &str,
) -> Result<Option<(String, Vec<gix::objs::tree::Entry>)>, String> {
    let entries = match fetch_tree_entries(t, hash)? {
        Some(entries) => entries,
        None => return Ok(None),
    };
    if entries
        .iter()
        .any(|e| entry_name(e) == CURRY_MARKER.as_bytes())
    {
        return Ok(None); // a curry node — handled by `curry_node`
    }
    let Some(image) = entries.iter().find(|e| entry_name(e) == b"image") else {
        return Ok(None); // no reserved `image` entry — not an args tree
    };
    // A git image rides embedded (the entry IS its tree, so the ref is the oid);
    // a `docker://` ref rides as a blob naming the registry ref.
    let base_ref = if image.mode.is_tree() {
        image.oid.to_string()
    } else {
        fetch_blob_string(t, &image.oid.to_string())?
    };
    let bound = entries
        .into_iter()
        .filter(|e| entry_name(e) != b"image")
        .collect();
    Ok(Some((base_ref, bound)))
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

/// Trigger compute for ArgTree `arg_tree` (its hash) and return the result's
/// `(type, hash)`. The server runs the container (resolving any promise it leaves
/// behind) and replies with the final `"<type> <hash>"`. (`req` is the query
/// param's historical name; its value is the arg-tree hash.)
/// Directory of the caller's git-ignored secrets store (design/secrets.md).
const SECRETS_DIR: &str = ".caos-secrets";

/// Minimum entropy length (chars) not flagged weak. A `secret-hash` is only
/// unguessable if the entropy is: below this, it's brute-forceable out of the
/// hash (like GitHub Actions refusing to mask short secrets).
const MIN_ENTROPY_LEN: usize = 16;

/// Fresh entropy for a secret: 128 random bits as 32 hex chars.
fn fresh_entropy() -> Result<String, String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open("/dev/urandom").map_err(|e| format!("opening /dev/urandom: {e}"))?;
    let mut buf = [0u8; 16];
    file.read_exact(&mut buf)
        .map_err(|e| format!("reading /dev/urandom: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// `caos-cli secrets [--check]` — tend the local `.caos-secrets` store: fill a
/// missing `entropy=` with fresh entropy and warn on a weak one, so a secret's
/// cache isolation is safe by default (design/secrets.md). `--check` writes
/// nothing and errors on any issue (a CI gate). Offline — reads/writes only the
/// local dir.
pub fn cli_secrets(check: bool) -> Result<(), String> {
    let dir = Path::new(SECRETS_DIR);
    if !dir.is_dir() {
        println!("no {SECRETS_DIR} directory — nothing to do");
        return Ok(());
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("reading {SECRETS_DIR}: {e}"))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    paths.sort();

    let mut issues = 0;
    for path in paths {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if name.starts_with('.') || !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("reading {name}: {e}"))?;
        let entropy = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("entropy=").map(str::trim));
        match entropy {
            None => {
                if check {
                    eprintln!("{name}: missing entropy");
                    issues += 1;
                } else {
                    let value = fresh_entropy()?;
                    let sep = if text.is_empty() || text.ends_with('\n') {
                        ""
                    } else {
                        "\n"
                    };
                    std::fs::write(&path, format!("{text}{sep}entropy={value}\n"))
                        .map_err(|e| format!("writing {name}: {e}"))?;
                    println!("{name}: added entropy");
                }
            }
            Some(value) if value.len() < MIN_ENTROPY_LEN => {
                // Never overwrite a user's value; just flag it.
                eprintln!(
                    "{name}: weak entropy ({} chars < {MIN_ENTROPY_LEN})",
                    value.len()
                );
                issues += 1;
            }
            Some(_) => {}
        }
    }
    if check && issues > 0 {
        return Err(format!("{issues} secret(s) with missing or weak entropy"));
    }
    if !check && issues > 0 {
        eprintln!("warning: {issues} secret(s) have weak entropy (edit them; not overwritten)");
    }
    Ok(())
}

/// A resolved secret from the caller's store: its value, its entropy (the
/// cache-isolation capability), and each reader resolved to a partial arg tree.
pub(crate) struct ClientSecret {
    name: String,
    value: String,
    entropy: String,
    readers: Vec<std::collections::BTreeMap<String, String>>,
}

/// Read and resolve the caller's `.caos-secrets` store (design/secrets.md):
/// each reader resolved HERE (via eval-path, against the store's pinned tree)
/// to a partial arg tree of name → oid — so the server only subset-matches,
/// never evals. Empty when there is no store.
pub(crate) fn build_secret_store(t: &dyn Transport) -> Result<Vec<ClientSecret>, String> {
    let dir = Path::new(SECRETS_DIR);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let pinned = secrets_pinned_tree(t, dir)?;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("reading {SECRETS_DIR}: {e}"))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    paths.sort();
    let mut store = Vec::new();
    for path in paths {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        // Skip dotfiles (`.tree`, editor backups) and non-files.
        if file_name.starts_with('.') || !path.is_file() {
            continue;
        }
        let (name, value, entropy, readers) = parse_local_secret(&file_name, &path)?;
        let readers = readers
            .iter()
            .map(|r| resolve_reader_client(t, &pinned, r))
            .collect::<Result<_, _>>()?;
        store.push(ClientSecret {
            name,
            value,
            entropy,
            readers,
        });
    }
    Ok(store)
}

/// Serialize the store for the `X-Caos-Secrets` header — a JSON array of
/// `{name, value, entropy, readers}`. Empty string for an empty store.
pub(crate) fn secret_store_header(store: &[ClientSecret]) -> String {
    if store.is_empty() {
        return String::new();
    }
    let array: Vec<serde_json::Value> = store
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "value": s.value,
                "entropy": s.entropy,
                "readers": s.readers,
            })
        })
        .collect();
    serde_json::to_string(&array).unwrap_or_default()
}

/// The `secret-hash` cache-isolation tag for an ArgTree whose base entries are
/// `base` (design/secrets.md): the git-blob digest of the `(name, entropy)`
/// pairs of the store's secrets whose readers match — `None` when none match.
/// Byte-identical to the server's [`crate::secrets`] side (both hash the shared
/// `caos_world::secret_hash_material`), so a computation shares one cache entry
/// whether the client or the server assembles it.
fn client_secret_hash(
    store: &[ClientSecret],
    base: &std::collections::BTreeMap<String, String>,
) -> Result<Option<String>, String> {
    let pairs: Vec<(&str, &str)> = store
        .iter()
        .filter(|s| s.readers.iter().any(|r| reader_subset(r, base)))
        .map(|s| (s.name.as_str(), s.entropy.as_str()))
        .collect();
    if pairs.is_empty() {
        return Ok(None);
    }
    let material = caos_world::secret_hash_material(&pairs);
    Ok(Some(hash_bytes("blob", &material)?.to_string()))
}

/// Is `reader` (a partial arg tree) a subset of `base`? (The client-side twin of
/// the server's match — kept identical so both compute the same `secret-hash`.)
fn reader_subset(
    reader: &std::collections::BTreeMap<String, String>,
    base: &std::collections::BTreeMap<String, String>,
) -> bool {
    reader.iter().all(|(name, oid)| base.get(name) == Some(oid))
}

/// Fold `secret-hash` into an existing arg tree `oid` when the carried store
/// grants it a secret — the caller-propagation mark (design/secrets.md): a
/// worker embedded (as a `:@=` arg, or returned by a `curry` expression) carries
/// its per-user identity, so whoever embeds it is per-user too. Unwraps any
/// curry layers, matches the store's readers against the flattened entries, and
/// on a match returns a flat args tree `{image, …bound, secret-hash}`; otherwise
/// returns `oid` unchanged. Idempotent (re-marking recomputes the same digest;
/// the merge dedups), and a no-op for an empty store.
pub(crate) fn mark_arg_tree(
    t: &dyn Transport,
    store: &[ClientSecret],
    oid: &str,
) -> Result<String, String> {
    if store.is_empty() {
        return Ok(oid.to_string());
    }
    let (image_ref, bound) = unwrap_curry(t, oid)?;
    let image_entry = image_arg_entry(t, &image_ref)?;
    let mut base: std::collections::BTreeMap<String, String> = bound
        .iter()
        .map(|e| {
            (
                String::from_utf8_lossy(entry_name(e)).into_owned(),
                e.oid.to_string(),
            )
        })
        .collect();
    base.insert("image".to_string(), image_entry.oid.to_string());
    let Some(digest) = client_secret_hash(store, &base)? else {
        return Ok(oid.to_string());
    };
    let secret_hash = gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Blob.into(),
        filename: caos_world::SECRET_HASH_ARG.as_bytes().to_vec().into(),
        oid: post_object(t, "blob", digest.as_bytes())?,
    };
    let entries = merge_entries(merge_entries(bound, vec![image_entry]), vec![secret_hash]);
    Ok(post_tree(t, entries)?.to_string())
}

/// The tree tree-relative readers resolve against: the `.tree` file in the store
/// (a hash or ref), else the caller's current working tree (ingested — which
/// also gets it onto the server so eval-path can walk it).
fn secrets_pinned_tree(t: &dyn Transport, dir: &Path) -> Result<String, String> {
    if let Ok(spec) = std::fs::read_to_string(dir.join(".tree")) {
        let spec = spec.trim();
        if !spec.is_empty() {
            return if is_hex_hash(spec) {
                Ok(spec.to_string())
            } else {
                resolve_ref(spec)
            };
        }
    }
    let (_, oid) = t
        .ingest_path(".")?
        .ok_or_else(|| "this transport cannot ingest the workspace tree for secrets".to_string())?;
    Ok(oid.to_string())
}

/// Parse one `.caos-secrets` file (repeated-key form) into `(name, value,
/// entropy, readers)`. `name` defaults to the filename; `value=`/`value:@=` set
/// the value; `reader=` accumulates; `entropy=` keys cache isolation (empty if
/// absent — the `caos secrets` tool fills it).
fn parse_local_secret(
    file_name: &str,
    path: &Path,
) -> Result<(String, String, String, Vec<String>), String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading secret {file_name}: {e}"))?;
    let mut name = file_name.to_string();
    let mut value: Option<String> = None;
    let mut entropy = String::new();
    let mut readers = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, val) = line
            .split_once('=')
            .ok_or_else(|| format!("secret {file_name}: line {line:?} is not key=value"))?;
        match key {
            "name" => name = val.trim().to_string(),
            "entropy" => entropy = val.trim().to_string(),
            "value" => value = Some(val.to_string()),
            "value:@" => {
                let file = path.parent().unwrap_or_else(|| Path::new(".")).join(val);
                let bytes = std::fs::read(&file)
                    .map_err(|e| format!("secret {file_name} value:@={val}: {e}"))?;
                value = Some(
                    String::from_utf8(bytes)
                        .map_err(|e| format!("secret {file_name} value not UTF-8: {e}"))?,
                );
            }
            "reader" => readers.push(val.trim().to_string()),
            other => return Err(format!("secret {file_name}: unknown key {other:?}")),
        }
    }
    let value = value.ok_or_else(|| format!("secret {file_name}: no value= line"))?;
    Ok((name, value, entropy, readers))
}

/// Resolve a reader — a single path/expression, no argument pins
/// (design/secrets.md: a reader names an *expression*; narrow by pointing at a
/// narrower one, not by pinning args here) — to the partial arg tree it stands
/// for: eval-path the path (so a flake/`.caos-expr` tool resolves to the same
/// arg tree the run uses), unwrap any curry layers, and take its entries. That
/// tree already carries whatever the expression bakes in (e.g. a curried
/// `worker1` script), so it is as specific as the expression is.
fn resolve_reader_client(
    t: &dyn Transport,
    pinned: &str,
    reader: &str,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    if reader.split_whitespace().count() != 1 {
        return Err(format!(
            "reader {reader:?} must be a single path (argument pins are not supported — \
             point at a narrower expression instead)"
        ));
    }
    let image = resolve_reader_image(t, pinned, reader.trim())?;
    let (base, bound) = unwrap_curry(t, &image)?;
    let mut entries = std::collections::BTreeMap::new();
    for entry in bound {
        entries.insert(
            String::from_utf8_lossy(entry_name(&entry)).into_owned(),
            entry.oid.to_string(),
        );
    }
    // The image entry wins over any like-named bound arg, mirroring assembly.
    entries.insert("image".to_string(), base);
    Ok(entries)
}

/// Resolve a reader's image token: a bare hash, or a path in the pinned tree
/// (via eval-path — so a flake/`.caos-expr` tool resolves to the same oid the
/// run uses).
///
/// A path only: there is no ambient library to name, so a reader says
/// `std/github-push` and it is read out of the tree, exactly as an expression
/// reaches a dependency. That is also why it converges with the run's own
/// resolution — the root `.caos-expr` deepens the tree, and the entry a reader
/// descends to is the same node a `DEEP-DEPS/<name>` mount points at.
fn resolve_reader_image(t: &dyn Transport, pinned: &str, expr: &str) -> Result<String, String> {
    if is_hex_hash(expr) {
        return Ok(expr.to_string());
    }
    // Empty store: a reader's own resolution must not be marked (its arg tree is
    // what the match compares against; marking it would be circular).
    let (_, oid) = eval::eval_path(t, pinned, expr, &[])?;
    Ok(oid)
}

fn request_compute(base: &str, arg_tree: &str, secrets: &str) -> Result<(String, String), String> {
    let url = format!("{}/run?req={}", base.trim_end_matches('/'), arg_tree);
    request_compute_url(&url, secrets)
}

fn request_compute_traced(
    base: &str,
    arg_tree: &str,
    trace_id: &str,
    secrets: &str,
) -> Result<(String, String), String> {
    let url = format!(
        "{}/run?req={arg_tree}&trace={trace_id}",
        base.trim_end_matches('/')
    );
    request_compute_url(&url, secrets)
}

/// Issue the compute `GET /run`, carrying the ephemeral secrets store in the
/// [`SECRETS_HEADER`] header when non-empty (design/secrets.md) — out of band
/// from the content-addressed ArgTree in the URL.
fn request_compute_url(url: &str, secrets: &str) -> Result<(String, String), String> {
    let mut request = minreq::get(url).with_header(caos_world::WORLD_HEADER, caos_world::WORLD);
    if !secrets.is_empty() {
        request = request.with_header(SECRETS_HEADER, secrets);
    }
    let response = request.send().map_err(|e| format!("GET {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        // Surface the server's response body — a 500 carries the worker's
        // failure output, which is what you actually need.
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
    let text = String::from_utf8(response.into_bytes())
        .map_err(|e| format!("server returned invalid UTF-8: {e}"))?;
    let (kind, hash) = text
        .trim()
        .split_once(' ')
        .ok_or_else(|| format!("server returned a malformed result: {:?}", text.trim()))?;
    if hash.is_empty() {
        return Err("server returned an empty result".to_string());
    }
    Ok((kind.to_string(), hash.to_string()))
}

fn request_compute_streamed(
    base: &str,
    arg_tree: &str,
    trace_id: &str,
    output: &mut (dyn Write + Send),
    secrets: &str,
) -> Result<(String, String), String> {
    let stream_url = format!("{}/trace/{trace_id}/stream", base.trim_end_matches('/'));
    let mut response = minreq::get(&stream_url)
        .with_header(caos_world::WORLD_HEADER, caos_world::WORLD)
        .send_lazy()
        .map_err(|e| format!("GET {stream_url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        let status = response.status_code;
        let reason = response.reason_phrase.clone();
        let mut body = String::new();
        let _ = response.read_to_string(&mut body);
        return Err(format!(
            "GET {stream_url}: server returned {status} {reason}: {}",
            body.trim()
        ));
    }

    std::thread::scope(|scope| {
        let trace = scope.spawn(|| -> Result<(), String> {
            let mut buffer = [0; 8192];
            loop {
                let read = response
                    .read(&mut buffer)
                    .map_err(|e| format!("reading trace: {e}"))?;
                if read == 0 {
                    break;
                }
                output
                    .write_all(&buffer[..read])
                    .and_then(|()| output.flush())
                    .map_err(|e| format!("writing trace: {e}"))?;
            }
            Ok(())
        });
        let result = request_compute_traced(base, arg_tree, trace_id, secrets);
        let trace_result = trace
            .join()
            .map_err(|_| "the trace stream thread panicked".to_string())?;
        let result = result?;
        trace_result?;
        Ok(result)
    })
}

fn valid_trace_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
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

#[cfg(test)]
mod git_transport_tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("caos-{label}-{}-{sequence}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        git_capture_in(args, None, cwd).unwrap()
    }

    fn init_repo(path: &Path) {
        git(path, &["init", "--quiet", "."]);
        git(path, &["config", "user.name", "CAOS Test"]);
        git(path, &["config", "user.email", "caos@example.invalid"]);
        git(path, &["config", "commit.gpgsign", "false"]);
    }

    fn commit_file(repo: &Path, name: &str, contents: &str, message: &str) -> String {
        std::fs::write(repo.join(name), contents).unwrap();
        git(repo, &["add", name]);
        git(repo, &["commit", "--quiet", "-m", message]);
        git(repo, &["rev-parse", "HEAD"]).trim().to_string()
    }

    #[test]
    fn transport_commands_stay_bound_to_the_discovered_repository() {
        let root = TestDir::new("bound-repository");
        let repo = root.path().join("repo");
        let nested = repo.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        init_repo(&repo);
        let expected_head = commit_file(&repo, "tracked", "temporary repo\n", "initial");

        let transport = GitTransport::discover(&nested).unwrap();

        assert_eq!(transport.work_dir(), repo.canonicalize().unwrap());
        assert_eq!(
            transport
                .resolve_revspec("HEAD")
                .unwrap()
                .unwrap()
                .to_string(),
            expected_head
        );
    }

    #[test]
    fn unreachable_server_error_names_the_url_and_remote() {
        let root = TestDir::new("unreachable-server");
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        git(&repo, &["remote", "add", CAOS_REMOTE, &url]);

        let error = GitTransport::discover(&repo)
            .unwrap()
            .ensure_server_reachable()
            .unwrap_err();

        assert!(error.contains(&format!("cannot reach the CAOS server at {url}")));
        assert!(error.contains("check that it is running"));
        assert!(error.contains("`caos` git remote"));
    }

    #[test]
    fn missing_caos_remote_error_explains_how_to_add_it() {
        let root = TestDir::new("missing-caos-remote");
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);

        let error = GitTransport::discover(&repo)
            .unwrap()
            .ensure_server_reachable()
            .unwrap_err();

        assert!(error.contains("no `caos` git remote"));
        assert!(error.contains("`git remote add caos <server-url>`"));
    }

    #[test]
    fn concurrent_object_fetches_do_not_touch_fetch_head() {
        let root = TestDir::new("concurrent-fetch");
        let remote = root.path().join("remote.git");
        let source = root.path().join("source");
        let client = root.path().join("client");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        git(&remote, &["init", "--quiet", "--bare", "."]);
        init_repo(&source);
        let base = commit_file(&source, "tracked", "base\n", "base");
        git(&source, &["branch", "-M", "main"]);
        let remote_path = remote.to_string_lossy();
        git(&source, &["remote", "add", "origin", &remote_path]);
        git(&source, &["push", "--quiet", "origin", "main"]);

        let client_path = client.to_string_lossy();
        git(
            root.path(),
            &[
                "clone",
                "--quiet",
                "--origin",
                CAOS_REMOTE,
                "--branch",
                "main",
                &remote_path,
                &client_path,
            ],
        );
        let target = commit_file(&source, "tracked", "updated\n", "updated");
        git(&source, &["push", "--quiet", "origin", "main"]);
        let fetch_head = client.join(".git/FETCH_HEAD");
        let sentinel = b"leave this file alone\n";
        std::fs::write(&fetch_head, sentinel).unwrap();

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let client = client.clone();
                let base = base.clone();
                let target = target.clone();
                std::thread::spawn(move || {
                    GitTransport::discover(client)?.fetch_object_negotiated(&target, &base)
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        assert_eq!(std::fs::read(fetch_head).unwrap(), sentinel);
        git(
            &client,
            &["cat-file", "-e", &format!("{target}^{{commit}}")],
        );
    }
}
