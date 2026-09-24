//! Generic CAOS client library shared by workers and host-side clients.
//!
//! * **`caos`** — the worker-side client baked setuid-root into worker images.
//!   It talks to the server over HTTP (`/object`) and runs the container
//!   `runner` (jobs arrive by long-poll; see `design/runner-protocol.md`). It
//!   normally records continuations for the server to resolve after the job;
//!   `sub-run` starts detached work inside the current server-side run context.
//!
//! Everything that doesn't depend on *how* objects move — the object model,
//! currying, args-tree assembly, CAS materialization, image import — lives here,
//! written against the [`Transport`] trait. The worker picks [`HttpTransport`];
//! host clients use [`GitTransport`]. Conversation semantics and presentation
//! live in the separate `caos-cli` crate.
//!
//! Every materialized path is tagged with the git hash it came from in the
//! `user.caos.hash` extended attribute — the top-level path with `<hash>`, and
//! each child of a tree with that entry's own oid. This is both the on-disk,
//! per-path, thread-safe mapping from CAS paths back to hashes, and what lets
//! `get` expand a placeholder later.

pub mod gitlinks;
pub mod import_git;
pub mod push_git;
pub mod timing;

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{IsTerminal, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use gix::objs::WriteTo;

mod eval;
mod watch;
pub use eval::cli_eval_path;

/// `run-tool <name | script> [--name=value ...]` — run a caos-tool by hand: fire
/// the tool as a caos job over this repo's tree, exactly what an agent's tool
/// invocation does. The tool gets the tracked worktree (dirty edits included)
/// as `--in`, plus the extra args verbatim.
///
/// A bare name is `caos-tools/<name>`, a DIRECTORY carrying a `.caos-expr`
/// (SPEC, "Tools"): evaluating it yields the tool's ArgTree — its worker image,
/// its script, and the `help` an agent registers it by — and the caller's args
/// curry onto that. **This is the same ArgTree the agent builds**, and it has to
/// be: the two callers share one cache entry, so a tool cannot behave
/// differently (or re-run) depending on who invoked it. The agent reaches the
/// same expression through `eval-path-then` because a worker may not block on
/// the runs an evaluation dispatches; here, at top level, it is a plain
/// `eval_path`.
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
    let dir = if tool.contains('/') {
        tool.trim_end_matches('/').to_string()
    } else {
        // A bare name is a caos-tools/<name> project tool, or — for the tools
        // that moved into std so the agent harness can offer them always
        // (caos-build, caos-test) — a std/<name> entry. Prefer the project tool
        // when both exist.
        let project = format!("caos-tools/{tool}");
        if Path::new(&format!("{project}/.caos-expr")).is_file() {
            project
        } else {
            format!("std/{tool}")
        }
    };
    if !Path::new(&format!("{dir}/.caos-expr")).is_file() {
        return Err(format!(
            "no such tool: {dir} is not a directory carrying a `.caos-expr`"
        ));
    }
    let name = Path::new(&dir)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("tool")
        .to_string();

    // The tool's ArgTree, from its own expression: the worker image it names,
    // its script, and its `help` — evaluated in the tracked worktree (dirty
    // edits included), so an edited tool runs edited.
    let (_, ws) = t
        .ingest_path(".")?
        .ok_or_else(|| "this client cannot ingest the source tree".to_string())?;
    let store = build_secret_store(t)?;
    let (kind, arg_tree) = eval::eval_path(t, &ws.to_string(), &dir, &store)?;
    if kind != "tree" {
        return Err(format!(
            "{dir}/.caos-expr evaluates to a {kind}, not an ArgTree"
        ));
    }

    // Do not add a `--bins` carrying the deploy's nix-built binaries. A tool
    // gets the tree under test and builds from source; handing it prebuilt host
    // binaries would couple every invocation to the deploy and let the suite
    // pass against something other than the tree it was given.
    let mut all: Vec<String> = vec!["--in:@=.".to_string()];
    all.extend(kvs.iter().cloned());
    let (kind, result) = run_request(t, &arg_tree, None, &all, &store)?;
    // The result's identity, on stdout, so a script can thread it onward — the
    // same "<kind> <hash>" line `caos-cli run` prints.
    println!("{kind} {result}");
    eprintln!("1126 {name}: {result}");
    report_conventions(t, &name, &result)
}

/// Evaluate a tool path against its selected source or conversation snapshot.
/// Locator resolution and secret marking remain client-side.
pub fn eval_tree_tool(
    t: &dyn Transport,
    root: &str,
    path: &str,
    store: &[ClientSecret],
) -> Result<String, String> {
    let (kind, oid) = eval::eval_path(t, root, path, store)?;
    if kind != "tree" {
        return Err(format!("{path} evaluates to a {kind}, not a tool ArgTree"));
    }
    t.ensure_pushed(&oid)?;
    Ok(oid)
}

/// Print a tool result's report conventions, reading ONLY the objects they
/// name: the top tree and a `report` blob, or the result itself when it is one.
/// A tool with no `report` (`build` returns an image) costs exactly one object.
fn report_conventions(t: &dyn Transport, name: &str, result: &str) -> Result<(), String> {
    let Some(entries) = fetch_tree_entries(t, result)? else {
        // Not a tree: the tool's whole answer IS the blob. Print it — the same
        // rendering the agent harness gives a blob result, so `run-tool
        // caos-test-result <hash>` by hand shows what the agent would have seen.
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
    // The full output is one `run-tool caos-test-result <hash>` away, which the
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

/// Fetch `GET /status/<arg_tree>` — the server's view of the work under an
/// ArgTree (SPEC.md "Tracing"). Returns the raw JSON, or None when the server
/// has nothing to show (a `null` body: the work is finished, or never ran here).
pub fn fetch_status(
    t: &dyn Transport,
    arg_tree: &str,
    all: bool,
) -> Result<Option<String>, String> {
    let base = t.server_url()?;
    let query = if all { "?all=1" } else { "" };
    let body = server_get(&base, &format!("/status/{arg_tree}{query}"))?;
    let text = String::from_utf8_lossy(&body).trim().to_string();
    Ok((text != "null" && !text.is_empty()).then_some(text))
}

/// `status [--all] <arg tree hash>` — print the work tree under an ArgTree.
///
/// The one-shot form of what a run shows live. Useful on its own for a run
/// happening in another terminal, and it is how the live display gets its data.
/// `--all` asks what HAPPENED instead: finished nodes are kept, a continuation
/// handler hangs off the node that promised it, and work this run REUSED rather
/// than performed is marked.
pub fn cli_status(t: &dyn Transport, arg_tree: &str, all: bool) -> Result<(), String> {
    match fetch_status(t, arg_tree, all)? {
        Some(json) => println!("{json}"),
        // Not an error: "nothing here" is a real answer, and for the live view
        // the commonest one — a finished run has no current work.
        None if all => eprintln!("nothing recorded under {arg_tree}"),
        None => eprintln!("no current work under {arg_tree}"),
    }
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

/// The current runner job's short-lived capability. A worker presents it to
/// `POST /sub-run`; the server accepts it only while that exact job is in
/// flight, and uses it to recover the job's server-held run context.
pub const JOB_NONCE_ENV: &str = "CAOS_JOB_NONCE";

/// Image-ref scheme marking an ordinary docker reference (vs. a git-image hash).
pub const DOCKER_SCHEME: &str = "docker://";

/// The reserved ArgTree entry naming the worker an ArgTree runs — and, since
/// there is no positional image anywhere in the grammar, the arg name every verb
/// reads its base out of: `run`/`curry`/`map-then` all take
/// `--base:<type>=<image>` like any other typed arg (design/flake-inputs.md).
/// Reserved: it is merged last, so it wins over a like-named user arg.
pub const BASE_ARG: &str = "base";

/// Marker entry naming a curry node: a CAS tree that pairs a `base` image ref
/// with an `args` subtree of bound arguments. `run`/`curry` expand it client-side
/// (merging the bound args under the call's args, then folding the base in as the
/// args' [`BASE_ARG`] entry) so the server only ever sees an ordinary args tree. The
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
    /// The server stores dependencies before owners, so a hit lets `store`
    /// prune the ordinary object graph. Gitlinks name separate histories and
    /// must be ensured as separate roots.
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

    /// Fetch the tree of a pinned commit into local storage and return its OID,
    /// or None if this transport has no repository to fetch into.
    ///
    /// `None` is the default because the worker's [`HttpTransport`] speaks only
    /// `/object`: there is no working repo and no `caos` remote to negotiate
    /// with. NOT because a worker lacks a network — it plainly has one (it
    /// reaches the server over HTTP, and `std/llm-client` and
    /// `std/flake-builder` both call out to the internet).
    ///
    /// What resolution being CLIENT-side buys is that the KEY IS CONTENT rather
    /// than a name — not determinism, which the mandatory full `rev` already
    /// gives wherever it is resolved. The ArgTree *is* the cache key, so the
    /// locator must become an oid before the request is formed; otherwise the
    /// URL sits in the key and two consumers pinning the same rev through
    /// different URLs get different keys for identical content. By the time a
    /// worker sees this arg it is an ordinary oid, resolved before the request
    /// existed (design/flake-inputs.md).
    fn fetch_git_ref(&self, _url: &str, _rev: &str) -> Result<Option<gix::ObjectId>, String> {
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
        let answer = server_call(
            &self.base,
            &ServerRequest {
                method: "POST",
                path: "/object/",
                headers: &[],
                body: Some(&body),
                timeout_secs: None,
            },
        )?;
        let text = std::str::from_utf8(&answer)
            .map_err(|e| format!("POST /object/: invalid response: {e}"))?;
        parse_oid(text)
    }

    fn get_object(&self, hash: &str) -> Result<(String, Vec<u8>), String> {
        let serialized = server_get(&self.base, &format!("/object/{hash}"))?;
        let (kind, content) = parse_object(&serialized)?;
        Ok((kind.to_string(), content.to_vec()))
    }

    fn has_object(&self, hash: &str) -> Result<bool, String> {
        // HEAD, so a 37 MB binary costs a status line to ask about. The server
        // answers 200 or 404 with no body.
        let response = server_request(
            &self.base,
            &ServerRequest {
                method: "HEAD",
                path: &format!("/object/{hash}"),
                headers: &[],
                body: None,
                timeout_secs: None,
            },
        )?;
        match response.status {
            200..=299 => Ok(true),
            404 => Ok(false),
            code => Err(format!(
                "HEAD /object/{hash}: server returned {code} {}",
                response.reason
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

    /// The git directory of the worktree. A per-checkout scratch location the
    /// `mcp serve` tool-registry cache lives under, so a `mcp warm` in the
    /// session-start hook and the `mcp serve` spawned right after it agree on
    /// one path without being told it.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
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
        server_request(
            &url,
            &ServerRequest {
                method: "GET",
                path: "/",
                headers: &[],
                body: None,
                timeout_secs: Some(TIMEOUT_SECS),
            },
        )
        .map(|_| ())
        .map_err(|error| {
            format!(
                "cannot reach the CAOS server at {url}: {error}\n\
                 check that it is running and that the `{CAOS_REMOTE}` git remote points to the right URL"
            )
        })
    }

    /// Run Git in this transport's bound working tree and return stdout.
    pub fn git_capture(&self, args: &[&str], index: Option<&Path>) -> Result<String, String> {
        git_capture_in(args, index, &self.work_dir)
    }

    /// Run Git in this transport's bound working tree and return its STDERR,
    /// which is where progress and summary lines land.
    fn git_capture_stderr(&self, args: &[&str]) -> Result<String, String> {
        git_capture_stderr_in(args, &self.work_dir)
    }

    /// Run `body` with this checkout holding the only claim on pushing `hash`.
    ///
    /// SINGLE-FLIGHT, because a push is expensive and `ensure_pushed` is
    /// check-then-act: two processes probe, both miss, and both send the whole
    /// closure. That is the same shape as the server's cache read-then-write
    /// (AGENTS.md), and it costs the same way — except the duplicated unit here
    /// is a git push, so in a repository of any size the loser spends minutes
    /// sending objects the winner is sending at the same moment, over the same
    /// link, each halving the other's bandwidth.
    ///
    /// It is not merely wasteful: git REJECTS the loser. Two `receive-pack`
    /// runs migrating the same objects out of quarantine collide, and the second
    /// dies `unable to migrate objects to permanent storage` — reproduced with
    /// two concurrent pushes of one 120 MB commit, where the loser transferred
    /// the entire pack before being rejected. `push_closure`'s retry then makes
    /// it correct, which is why it stays invisible: the only symptom is time.
    ///
    /// WHAT THIS DOES NOT EXPLAIN, so that nobody reads it as settled: a cloud
    /// session spent 148s on one push of a 415 MB repository and this was the
    /// suspect, on the evidence that every process logged exactly one
    /// `push-failed`. That evidence turned out to be a different thing entirely —
    /// `fatal: bad tree object`, the ordinary unreadable-graph fallback to
    /// `hand_over_graph` (`tests/push-closure`), which is by design and happens
    /// once per process for reasons that have nothing to do with concurrency.
    /// The 148s stall remains unattributed. This function is justified by the
    /// reproduction above, not by that measurement.
    ///
    /// Processes, not threads, so the claim is a file. Bounded, and a stale claim
    /// is ignored rather than honoured — a killed pusher must not park every
    /// later one — and if the wait runs out we push anyway, which is exactly the
    /// behaviour this replaces.
    fn with_push_claim<T>(&self, hash: &str, body: impl FnOnce() -> T) -> T {
        /// Long enough to cover a large first push over a relayed path (measured:
        /// 300 MB in 4m56s), because waiting out the winner is strictly cheaper
        /// than racing it — the waiter's own push becomes a probe that skips.
        const WAIT: std::time::Duration = std::time::Duration::from_secs(600);
        const STEP: std::time::Duration = std::time::Duration::from_millis(200);

        let path = self.git_dir.join(format!("caos-push-{hash}.claim"));
        let deadline = std::time::Instant::now() + WAIT;
        let mut held = false;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => {
                    held = true;
                    break;
                }
                // Someone else is pushing this. Wait for them rather than joining
                // in -- unless the claim is old enough that its owner is gone.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .map(|at| at.elapsed().unwrap_or_default() > WAIT)
                        .unwrap_or(true);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        timing::record(
                            "push-claim-expired",
                            &format!("{hash}: waited {}s for another pusher", WAIT.as_secs()),
                        );
                        break;
                    }
                    std::thread::sleep(STEP);
                }
                // A claim we cannot create is not a reason to refuse the push.
                Err(_) => break,
            }
        }
        let outcome = body();
        if held {
            let _ = std::fs::remove_file(&path);
        }
        outcome
    }
}

impl Transport for GitTransport {
    fn put_object(&self, kind: &str, content: &[u8]) -> Result<gix::ObjectId, String> {
        let write = |repo: &gix::Repository| {
            match kind {
                "blob" => repo
                    .write_blob(content)
                    .map(|id| id.detach())
                    .map_err(|e| format!("writing blob: {e}")),
                "tree" => {
                    // Validate the canonical tree encoding, then write it as a real
                    // tree object so its hash is a genuine git tree hash.
                    let tree = gix::objs::TreeRef::from_bytes(content, repo.object_hash())
                        .map_err(|e| format!("invalid tree: {e}"))?;
                    repo.write_object(&tree)
                        .map(|id| id.detach())
                        .map_err(|e| format!("writing tree: {e}"))
                }
                "commit" => {
                    // Validate the commit encoding, then store the raw bytes (not a
                    // re-encoding), so the hash matches the bytes exactly — the same
                    // rule the server's `post_object` applies.
                    gix::objs::CommitRef::from_bytes(content, repo.object_hash())
                        .map_err(|e| format!("invalid commit: {e}"))?;
                    gix::objs::Write::write_buf(&repo.objects, gix::object::Kind::Commit, content)
                        .map_err(|e| format!("writing commit: {e}"))
                }
                other => Err(format!("cannot store object of kind {other}")),
            }
        };
        write(&self.repo).or_else(|original| {
            // Fetches can add more packfiles than this long-lived handle's
            // fixed slotmap can hold. Reopen against the current disk state
            // before failing a write, as get_object already does for reads.
            let repo = gix::open(&self.git_dir).map_err(|_| original)?;
            write(&repo)
        })
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
        let server = self.server_url()?;
        let serialized = server_get(&server, &format!("/object/{hash}"))?;
        let (kind, content) = parse_object(&serialized)?;
        // Write it into the local repo so the next ask — and the next run — is
        // a local hit. put_object validates that the bytes hash to `hash`.
        let stored = self.put_object(kind, content)?;
        if stored != oid {
            return Err(format!(
                "{server} returned an object hashing to {stored}, not {hash}"
            ));
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
        // ASK BEFORE PUSHING. A push is idempotent but not free: it forks git,
        // and its first act is `POST /info/refs?service=git-receive-pack`, which
        // downloads the server's ENTIRE ref advertisement — one
        // `refs/caos/req/<oid>` per request anyone has ever made, a set that only
        // grows. A `HEAD /object/<hash>` is a status line. Every `run` an
        // expression dispatches lands here, so a resolution that is nothing but
        // cache hits was paying that advertisement a dozen times over.
        //
        // The server admits objects only after their dependencies, and verifies
        // complete history before publishing a Git transfer. Presence therefore
        // certifies closure for commits as well as ordinary trees and blobs.
        // Gitlinks are separate roots and are ensured by resolve_commit_arg.
        //
        // What is given up is the REF, which `hand_over_graph`'s raw posts do
        // not create either: an object the server got that way stops being a
        // negotiation base for a later delta push. It was never one — the
        // client could not read its graph, which is why it went that route.
        // COUNTED, not logged one line per call. "The server already had it" is
        // the common answer -- a single resolve takes this branch a dozen or more
        // times -- and a journal line each would push the one expensive push out
        // of any readable tail. The count is what carries the information (it
        // proves the cheap path was taken), and [`push_counts`] reports it
        // alongside the phases that enclose these calls.
        //
        // A SLOW probe is still logged individually, because it stops being
        // routine: this is one `HEAD /object/<hash>` round trip, so a slow one
        // is a statement about the transport, not about the object.
        let started = std::time::Instant::now();
        let held = self.server_holds(hash);
        let probe = started.elapsed().as_secs_f64();
        if probe >= 0.5 {
            timing::record(
                "slow-object-probe",
                &format!("{hash}: {probe:.1}s to ask whether the server holds it"),
            );
        }
        if held {
            PUSHES_SKIPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(());
        }
        // RE-PROBE INSIDE THE CLAIM, which is the half of single-flighting that
        // actually saves the work: the waiter's answer to "does the server hold
        // it" was computed before the winner's push and is stale by exactly the
        // thing it needs to know. The server's closure invariant makes this
        // sound for commits, trees, and blobs, just like the first probe.
        self.with_push_claim(hash, || {
            if self.server_holds(hash) {
                PUSHES_SKIPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                timing::record(
                    "push-avoided",
                    &format!("{hash}: another process delivered it while we waited"),
                );
                return Ok(());
            }
            PUSHES_SENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.push_closure(hash)
        })
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

    fn fetch_git_ref(&self, url: &str, rev: &str) -> Result<Option<gix::ObjectId>, String> {
        let tree_ref = format!("refs/caos/locator-trees/{rev}");
        for name in [rev, tree_ref.as_str()] {
            if let Ok(tree) = self.git_capture(
                &["rev-parse", "--verify", &format!("{name}^{{tree}}")],
                None,
            ) {
                return parse_oid(tree.trim()).map(Some);
            }
        }
        // A locator needs a snapshot, not history. Keep the shallow boundary
        // and incomplete commit in a disposable repo; copy only its tree closure.
        let scratch = scratch_dir()?;
        let result = (|| {
            let git = |args: &[&str]| git_capture_in(args, None, &scratch);
            git(&["init", "--bare", "--quiet"])?;
            git(&[
                "fetch",
                "--quiet",
                "--depth=1",
                "--no-tags",
                "--no-write-fetch-head",
                "--",
                url,
                rev,
            ])?;
            let tree = git(&[
                "rev-parse",
                "--verify",
                &format!("{rev}^{{commit}}^{{tree}}"),
            ])?;
            let tree = tree.trim();
            let pack = scratch.join("tree.pack");
            let mut writer = std::process::Command::new("git")
                .current_dir(&scratch)
                .args(["pack-objects", "--stdout", "--revs"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::fs::File::create(&pack).map_err(|e| e.to_string())?)
                .spawn()
                .map_err(|e| e.to_string())?;
            let written = writeln!(writer.stdin.take().unwrap(), "{tree}");
            let status = writer.wait().map_err(|e| e.to_string())?;
            written.map_err(|e| e.to_string())?;
            if !status.success() {
                return Err("packing locator tree failed".into());
            }
            let status = std::process::Command::new("git")
                .current_dir(&self.work_dir)
                .args(["index-pack", "--stdin"])
                .stdin(std::fs::File::open(&pack).map_err(|e| e.to_string())?)
                .stdout(std::process::Stdio::null())
                .status()
                .map_err(|e| e.to_string())?;
            if !status.success() {
                return Err("storing locator tree failed".into());
            }
            // The ref maps the pin to its tree across processes and retains the
            // complete tree for GC. No foreign commit enters the caller's store.
            self.run_git(&["update-ref", &tree_ref, tree])?;
            parse_oid(tree).map(Some)
        })();
        let _ = std::fs::remove_dir_all(scratch);
        result.map_err(|e| format!("fetching {rev} from {url}: {e}"))
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
    /// Push an object and its reachable graph under its content-addressed
    /// request ref. Callers perform the server-presence probe first.
    fn push_closure(&self, hash: &str) -> Result<(), String> {
        // Content-addressed ref: clobber-free across clients, idempotent (a
        // re-push of the same content is a no-op), and it persists as the
        // negotiation base for the next push, so an edited tree ships only its
        // delta. The push carries the whole object graph reachable from `hash`.
        let push = || self.push_req_ref(hash);

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
        let mut probed = false;
        for attempt in 0..4 {
            match push() {
                Ok(_) => return Ok(()),
                Err(e) => last = e,
            }
            // A graph we cannot READ is not the create race, and no retry will
            // fix it — see `hand_over_graph`. Decided by asking git to walk the
            // graph rather than by matching its wording, for the same reason the
            // retry above is unconditional: the message varies by version.
            // Probed once, and only after a failure, so a healthy push pays
            // nothing.
            if !probed {
                probed = true;
                if !self.graph_readable(hash) {
                    return self.hand_over_graph(hash);
                }
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

    fn push_req_ref(&self, hash: &str) -> Result<(), String> {
        let refspec = format!("{hash}:refs/caos/req/{hash}");
        // NOT `--quiet`, because what this push COST is the question it is most
        // often asked. git's own progress lines are the only place the answer
        // exists -- `Total N (delta D), reused R` says whether the server's
        // advertisement was used as a negotiation base or whether we just
        // shipped the entire history again, and no timing alone distinguishes
        // those. `--porcelain` keeps stdout machine-shaped; the counts are on
        // stderr either way, which `git_capture_in` already collects.
        //
        // Progress is forced on because stderr is a pipe here, and git suppresses
        // it when not on a terminal -- without this the summary line is simply
        // absent and the measurement silently reports nothing.
        // NEGOTIATE, BUT ONLY FOR A COMMIT. `push.negotiate` runs a `fetch
        // --negotiate-only` first, so the server can say which of this history it
        // already holds and the pack carries only the rest. That is the ONLY
        // mechanism that works here, because the alternative — excluding what the
        // server advertises — needs a ref pointing into the history, and request
        // refs are pruned after ten minutes (`spawn_request_ref_pruner`). So a
        // session that pushes a commit a day after the last one finds nothing to
        // exclude and re-sends the whole closure: 318 objects against 3, measured.
        //
        // Decided by OBJECT TYPE rather than by which caller asked, because the
        // expensive push is `ensure_code_commit`'s and it comes through
        // `ensure_pushed` like any tree — scoping by call path would miss exactly
        // the case this is for. A tree or blob has no history to find common
        // ancestors in, so negotiating one buys nothing and costs a round trip.
        //
        // `protocol.version=2` is asked for explicitly: `--negotiate-only`
        // requires v2, and while it is git's default since 2.26 a client that set
        // it otherwise would silently get `warning: push negotiation failed;
        // proceeding anyway` and the full closure back.
        let negotiate = self.is_commit_object(hash);
        let mut args: Vec<&str> = Vec::new();
        if negotiate {
            args.extend(["-c", "protocol.version=2", "-c", "push.negotiate=true"]);
        }
        args.extend(["push", "--porcelain", "--progress", CAOS_REMOTE, &refspec]);

        let started = std::time::Instant::now();
        let outcome = self.git_capture_stderr(&args);
        let elapsed = started.elapsed().as_secs_f64();
        match outcome {
            Ok(stderr) => {
                let summary = stderr
                    .lines()
                    .find(|line| line.starts_with("Total "))
                    .unwrap_or("no object summary")
                    .trim()
                    .to_string();
                // The transport condition, from the helper's own trace. A RELAYED
                // push is the cloud's permanent condition -- measured at ~1 MB/s
                // against 49 MB/s direct -- so whether a push was relayed is most
                // of what its duration means.
                let path = stderr
                    .lines()
                    .rev()
                    .find_map(|line| line.trim().strip_prefix("caos-iroh: path "))
                    .map(|path| format!(" over {path}"))
                    .unwrap_or_default();
                timing::record("push", &format!("{hash} in {elapsed:.1}s{path}: {summary}"));
                Ok(())
            }
            Err(error) => {
                // WITH THE REASON. A failed push that records only its duration
                // says the one thing already obvious from the next line's
                // duration, and withholds the only thing that identifies the
                // fault: a 148s failure in a cloud session was indistinguishable
                // from a timeout, a rejection and a dropped connection, and the
                // container was gone before anyone could ask.
                //
                // Newlines collapsed because a journal line is a line -- git's
                // failure text is several, and the tail reader splits on them.
                let reason: String = error
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
                    .join(" | ");
                timing::record(
                    "push-failed",
                    &format!("{hash} after {elapsed:.1}s: {reason}"),
                );
                Err(error)
            }
        }
    }

    /// Does the server already hold `hash`? One `HEAD /object/<hash>` — a status
    /// line, no body, no subprocess, no ref advertisement. The probe
    /// [`Transport::ensure_pushed`] skips a push on.
    ///
    /// Returns a bool rather than a `Result` because the answer is only ever
    /// used to SKIP work, so every uncertainty must resolve to "push anyway":
    ///
    /// - a `caos` remote that is a plain git URL has no `/object` endpoint at
    ///   all (`post_object_http` says so at length) but pushes perfectly well,
    ///   so a non-HTTP remote is not an error here, it is simply not probeable;
    /// - a transient HTTP failure is the push's to report. Failing here would
    ///   replace the push's own diagnosis with this probe's.
    ///
    /// Note this is not [`Transport::has_object`], which for this transport is
    /// deliberately LOCAL presence — the two questions have different answers
    /// and different callers.
    fn server_holds(&self, hash: &str) -> bool {
        let Ok(server) = self.server_url() else {
            return false;
        };
        server_request(
            &server,
            &ServerRequest {
                method: "HEAD",
                path: &format!("/object/{hash}"),
                headers: &[],
                body: None,
                timeout_secs: None,
            },
        )
        .is_ok_and(|response| (200..300).contains(&response.status))
    }

    /// Is `hash` a COMMIT in the local object store?
    ///
    /// Read from the odb rather than shelled out, and false for anything it
    /// cannot answer: the only caller uses it to decide whether to negotiate, so
    /// an uncertain answer must mean "don't", which is today's behaviour.
    fn is_commit_object(&self, hash: &str) -> bool {
        parse_oid(hash)
            .ok()
            .and_then(|oid| self.repo.find_object(oid).ok())
            .is_some_and(|object| object.kind == gix::object::Kind::Commit)
    }

    /// Can git walk everything reachable from `hash` in THIS repo?
    ///
    /// `--quiet` so a large tree costs a walk and not a printed listing. Used
    /// only after a push has already failed, to tell an unreadable graph apart
    /// from a transient failure without pattern-matching git's error text.
    fn graph_readable(&self, hash: &str) -> bool {
        self.git_capture(&["rev-list", "--objects", "--quiet", hash], None)
            .is_ok()
    }

    /// Read `hash` from the LOCAL object store only — `None` if we don't have
    /// it. Deliberately unlike [`Transport::get_object`], which falls back to
    /// the server: here the whole question is what we hold.
    fn read_local(&self, hash: &str) -> Result<Option<(String, Vec<u8>)>, String> {
        let oid = parse_oid(hash)?;
        if let Ok(object) = self.repo.find_object(oid) {
            return Ok(Some((object.kind.to_string(), object.data.clone())));
        }
        // Re-open for packs written after this handle was cached (a fetch), the
        // same hazard `get_object` guards against.
        if let Ok(repo) = gix::open(&self.git_dir) {
            if let Ok(object) = repo.find_object(oid) {
                return Ok(Some((object.kind.to_string(), object.data.clone())));
            }
        }
        Ok(None)
    }

    /// Get the graph rooted at `hash` onto the server when `git push` CANNOT,
    /// because the client references objects it does not hold.
    ///
    /// That state is ordinary, not corruption: an ArgTree names its base image
    /// as a real tree entry, but the client only ever fetched that image's ROOT
    /// (one object, per `get_object`'s laziness) — every interior object exists
    /// solely on the server. git will not use the remote's copy as a boundary
    /// (it drops negative tips it lacks), so it tries to pack what it cannot
    /// read and dies. It only bites when no advertised ref reaches the image:
    /// a flake-built image is pinned as a run result and so is covered, while a
    /// curry's base is a BLOB naming the hash, leaving the unwrapped runner-pool
    /// image reachable from nothing.
    ///
    /// Walk ordinary tree entries so locally available children arrive first.
    /// Commits are posted as supplied: Git on the server rejects incomplete
    /// history. This fallback does not assemble missing commit ancestry.
    fn hand_over_graph(&self, hash: &str) -> Result<(), String> {
        let Some((kind, content)) = self.read_local(hash)? else {
            return Ok(()); // The server validates dependencies before storing the owner.
        };
        if kind == "tree" {
            let tree = gix::objs::TreeRef::from_bytes(&content, self.repo.object_hash())
                .map_err(|e| format!("malformed tree {hash}: {e}"))?;
            for entry in tree.entries {
                // Gitlinks are not reachability-traversed, so a commit arg's
                // closure never rode in this push anyway (`resolve_commit_arg`
                // ships it separately).
                if entry.mode.is_commit() {
                    continue;
                }
                let child = entry.oid.to_string();
                if self.graph_readable(&child) {
                    // `ensure_pushed`, not the raw push: a child ref is subject
                    // to the same create race as any other, and skipping its
                    // retry turned a concurrent suite into "cannot lock ref …:
                    // reference already exists".
                    self.ensure_pushed(&child)?;
                } else {
                    self.hand_over_graph(&child)?;
                }
            }
        }
        self.post_object_http(&kind, &content)
    }

    /// Hand the server ONE object's bytes over the HTTP object API, in the
    /// `<type> <size>\0<content>` framing `HttpTransport::put_object` uses.
    fn post_object_http(&self, kind: &str, content: &[u8]) -> Result<(), String> {
        let mut body = format!("{kind} {}\0", content.len()).into_bytes();
        body.extend_from_slice(content);
        let server = self.server_url()?;
        // Says which requirement this is, on top of whatever the transport
        // itself reports: a `caos` remote that is a plain git URL serves git
        // perfectly well and has no `/object` endpoint at all, and that is the
        // one failure worth explaining rather than relaying.
        server_call(
            &server,
            &ServerRequest {
                method: "POST",
                path: "/object/",
                headers: &[],
                body: Some(&body),
                timeout_secs: None,
            },
        )
        .map_err(|error| {
            format!(
                "cannot hand objects to the `{CAOS_REMOTE}` remote: completing a push whose local \
                 graph is incomplete needs the server's `/object` endpoint. {error}"
            )
        })?;
        Ok(())
    }

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
    pub fn fetch_object(&self, hash: &str) -> Result<(), String> {
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
    /// a repo with real history re-downloads the whole source tree closure every
    /// turn (measured: ~10s of index-pack CPU per turn on a large repo).
    #[cfg(test)]
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
/// Run git and hand back its STDERR on success, where git writes progress and
/// summary lines. [`git_capture_in`] collects stderr too but keeps it only for
/// the failure message, so a caller that wants to measure a SUCCESSFUL command
/// has nowhere to read from.
fn git_capture_stderr_in(args: &[&str], cwd: &Path) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(args)
        // TRACE ON, unconditionally, for the one command whose stderr we keep.
        //
        // `git-remote-caos` is a separate process and cannot reach the phase
        // journal, so the only channel it has is git's stderr -- which this
        // function is already collecting and, until now, was discarding on
        // success and quoting only on failure. Under the trace it names the PATH
        // (relayed or direct) and the bytes it moved in each direction, which is
        // the difference between "the push failed" and "the push failed after
        // moving 12 MB over a relayed path".
        //
        // It costs three lines of text that no one sees unless something is
        // being diagnosed. That is cheap next to the alternative, which is a
        // cloud container that no longer exists.
        .env("CAOS_IROH_TRACE", "1")
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("running git {}: {e}", args.join(" ")))?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(format!("git {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(stderr)
}

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

/// Scheme of the iroh ticket transport (design/iroh-transport.md). A server
/// named this way is reached by TICKET rather than by address.
pub const TICKET_SCHEME: &str = "caos://";

/// One request to the caos server, independent of how it travels.
pub struct ServerRequest<'a> {
    pub method: &'a str,
    /// Absolute path and query, `/object/<hash>` style, joined onto the base.
    pub path: &'a str,
    pub headers: &'a [(&'a str, String)],
    pub body: Option<&'a [u8]>,
    /// Wall-clock cap, for the few callers that would rather fail than wait.
    /// None for the rest — `GET /run` blocks for as long as the work takes.
    pub timeout_secs: Option<u64>,
}

/// What came back. The status is kept rather than turned into an error here, so
/// each caller decides what a 404 means (absent, for `has_object`; a failure,
/// for a fetch).
pub struct ServerResponse {
    pub status: u16,
    pub reason: String,
    pub body: Vec<u8>,
}

/// A transport for [`TICKET_SCHEME`] URLs, installed at runtime.
///
/// INSTALLED RATHER THAN LINKED, and that is a deliberate constraint on this
/// crate rather than an abstraction for its own sake. The implementation is
/// `caos-iroh`, whose dependency tree is iroh, quinn and tokio; this crate is
/// also the worker's `/bin/caos`, baked setuid into every worker image. Cargo
/// unifies features across workspace members in one `cargo build --workspace`,
/// so *depending* on that crate — however carefully gated — would put a QUIC
/// stack in every worker image. A host binary installs one; a worker never does,
/// and never needs to, since the server it is handed is always an address on the
/// docker network.
pub trait TicketTransport: Send + Sync {
    fn request(&self, base: &str, request: &ServerRequest) -> Result<ServerResponse, String>;
}

static TICKET_TRANSPORT: std::sync::OnceLock<Box<dyn TicketTransport>> = std::sync::OnceLock::new();

/// Install the transport for `caos://` servers. Call once, early, from a host
/// binary's `main`. A second call is ignored, so two entry points into the same
/// process cannot fight over it.
pub fn install_ticket_transport(transport: Box<dyn TicketTransport>) {
    let _ = TICKET_TRANSPORT.set(transport);
}

/// Perform `request` against the caos server at `base`, over whichever transport
/// the base URL names.
///
/// THE ONE PLACE that knows how to reach the server, which is what lets a second
/// transport exist at all: every `/object`, `/run`, `/status`, `/sub-run` and
/// `/trace/child` call in this crate goes through here.
/// How many times `ensure_pushed` found the object already on the server, and
/// how many times it had to send it. See the note in `ensure_pushed`.
static PUSHES_SKIPPED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static PUSHES_SENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `(already on the server, actually pushed)` so far in this process, for a
/// phase line to report. The ratio is the answer to "did we re-send a repo the
/// server already had".
pub fn push_counts() -> (usize, usize) {
    (
        PUSHES_SKIPPED.load(std::sync::atomic::Ordering::Relaxed),
        PUSHES_SENT.load(std::sync::atomic::Ordering::Relaxed),
    )
}

pub fn server_request(base: &str, request: &ServerRequest) -> Result<ServerResponse, String> {
    // The FIRST request of a process is always recorded, and the rest only when
    // they are slow. That asymmetry is the measurement: over a ticket the first
    // request pays the whole dial -- relay lookup, TLS, QUIC handshake -- and
    // every later one rides the open connection, so an average hides exactly the
    // cost that matters. It is also the cost that decided a session: a 5s
    // reachability probe is a budget on THIS number, and nothing recorded it.
    static FIRST_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    const SLOW: f64 = 2.0;

    let started = std::time::Instant::now();
    let answer = server_request_inner(base, request);
    let elapsed = started.elapsed().as_secs_f64();
    let first = !FIRST_DONE.swap(true, std::sync::atomic::Ordering::Relaxed);
    if first || elapsed >= SLOW || answer.is_err() {
        let outcome = match &answer {
            Ok(response) => format!("{} ({} bytes)", response.status, response.body.len()),
            Err(error) => format!("failed: {error}"),
        };
        timing::record(
            if first {
                "server-request-first"
            } else {
                "server-request"
            },
            &format!(
                "{} {} in {elapsed:.1}s -> {outcome}",
                request.method, request.path
            ),
        );
    }
    answer
}

fn server_request_inner(base: &str, request: &ServerRequest) -> Result<ServerResponse, String> {
    let base = base.trim_end_matches('/');
    // THE WORLD TAG GOES ON EVERY REQUEST, whichever transport carries it, and
    // it is stamped HERE rather than per-branch for that reason. A request
    // without it is allowed through (`caos_world`: git's own traffic and health
    // probes have none), so a transport that forgot to stamp it would not fail —
    // it would silently let a host client drive a test stack, which is the exact
    // crossing the tag exists to prevent.
    let mut headers: Vec<(&str, String)> =
        vec![(caos_world::WORLD_HEADER, caos_world::WORLD.to_string())];
    headers.extend(request.headers.iter().map(|(n, v)| (*n, v.clone())));
    let request = &ServerRequest {
        headers: &headers,
        ..*request
    };

    if base.starts_with(TICKET_SCHEME) {
        let transport = TICKET_TRANSPORT.get().ok_or_else(|| {
            format!(
                "the caos server is a ticket ({TICKET_SCHEME}…) and this build has no transport \
                 for it: git reaches such a server through `git-remote-caos`, but this binary \
                 cannot. Point it at the server's HTTP URL"
            )
        })?;
        return transport.request(base, request);
    }
    if !base.starts_with("http://") && !base.starts_with("https://") {
        return Err(format!(
            "the caos server {base:?} is neither an HTTP URL nor a ticket ({TICKET_SCHEME}…): a \
             plain git remote serves git, but has no `/object` or `/run` endpoint"
        ));
    }

    let url = format!("{base}{}", request.path);
    let mut http = match request.method {
        "GET" => minreq::get(&url),
        "HEAD" => minreq::head(&url),
        "POST" => minreq::post(&url),
        other => return Err(format!("unsupported method {other}")),
    };
    for (name, value) in request.headers {
        http = http.with_header(*name, value);
    }
    if let Some(body) = request.body {
        http = http.with_body(body.to_vec());
    }
    if let Some(seconds) = request.timeout_secs {
        http = http.with_timeout(seconds);
    }
    let response = http
        .send()
        .map_err(|e| format!("{} {url}: {e}", request.method))?;
    Ok(ServerResponse {
        status: response.status_code as u16,
        reason: response.reason_phrase.clone(),
        body: response.into_bytes(),
    })
}

/// A request whose non-2xx answer is an error, with the server's body attached.
///
/// That body matters: for `/run` a 500 carries the worker's failure output,
/// which is the thing you actually need to read.
fn server_call(base: &str, request: &ServerRequest) -> Result<Vec<u8>, String> {
    let response = server_request(base, request)?;
    if !(200..300).contains(&response.status) {
        let body = String::from_utf8_lossy(&response.body);
        let body = body.trim();
        let detail = if body.is_empty() {
            String::new()
        } else {
            format!(":\n{body}")
        };
        return Err(format!(
            "{} {base}{}: server returned {} {}{detail}",
            request.method, request.path, response.status, response.reason
        ));
    }
    Ok(response.body)
}

/// GET `<base><path>`, returning the raw body. Non-2xx responses are errors.
fn server_get(base: &str, path: &str) -> Result<Vec<u8>, String> {
    server_call(
        base,
        &ServerRequest {
            method: "GET",
            path,
            headers: &[],
            body: None,
            timeout_secs: None,
        },
    )
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
    write_file_with_mode(target, hash, kind, data, exec)
}

fn write_file_with_mode(
    target: &Path,
    hash: &str,
    kind: &str,
    data: &[u8],
    exec: bool,
) -> Result<(), String> {
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
pub fn cas_kind(path: &str) -> Result<(), String> {
    let target = validate_descendant(&cas_dir(), path)?;
    println!("{}", result_kind(&target)?);
    Ok(())
}

pub fn cas_hash(path: &str) -> Result<(), String> {
    let cas = cas_dir();
    let target = validate_descendant(&cas, path)?;
    println!("{}", read_hash(&target)?);
    Ok(())
}

/// `forward <src-cas-path> <dst-cas-path>` — record the object already named
/// by `src` at a fresh CAS path `dst`, preserving its result kind. This is the
/// zero-copy pass-through a continuation callback needs when its own result is
/// exactly the prior step's blob/tree/commit.
pub fn forward(src: &str, dst: &str) -> Result<(), String> {
    let cas = cas_dir();
    let source = validate_descendant(&cas, src)?;
    let target = validate_target(&cas, dst)?;
    probe_xattr(&cas)?;
    let kind = result_kind(&source)?;
    match kind.as_str() {
        "blob" | "tree" | "commit" => write_placeholder(&target, &kind, &read_hash(&source)?),
        other => Err(format!("cannot forward a {other} result")),
    }
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
/// The old single pass posted every object as it hashed it. `caos-tools/build/worker.sh`
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
    Commit(Vec<u8>, Box<Hashed>),
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
        return gitlinks::commit(
            cas_real,
            path,
            Hashed {
                mode: EntryKind::Tree.into(),
                oid,
                body: Body::Dir(buf, children),
            },
        );
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
        Body::Commit(encoded, tree) => {
            send(t, tree)?;
            refuse_if_leaks(encoded, "a commit")?;
            t.put_object("commit", encoded)?
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
            // `--name:hash=oid` — an object the server already holds (an earlier
            // result), referenced by oid: a tree or a blob. Verified server-side
            // so a typo fails here, not as a bad materialization in the worker.
            ArgType::Hash => {
                let (kind, _) = t
                    .get_object(value)
                    .map_err(|e| format!("`{name}`: object {value}: {e}"))?;
                let mode = match kind.as_str() {
                    "tree" => EntryKind::Tree,
                    "blob" => EntryKind::Blob,
                    other => {
                        return Err(format!(
                            "`{name}`: {value} is a {other}; :hash= names a tree or blob"
                        ))
                    }
                };
                (mode.into(), parse_oid(value)?)
            }
            // `--name:docker=ref` — a docker image ref, stored as the blob
            // `docker://<ref>` (the representation the server expects).
            ArgType::Docker => (
                EntryKind::Blob.into(),
                post_object(t, "blob", format!("{DOCKER_SCHEME}{value}").as_bytes())?,
            ),
            // `--name:@@=ref` — a tree in another repo, fetched here (client
            // only) and reduced to its oid, so what the request carries is
            // indistinguishable from a local path arg.
            ArgType::Remote => {
                resolve_remote_arg(t, value, &[]).map_err(|e| format!("`{name}`: {e}"))?
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
    // Pushing the containing ArgTree sends neither this commit nor its history.
    t.ensure_pushed(&oid.to_string())?;
    Ok(oid)
}

/// The one arg-type vocabulary and its parser now live in the shared
/// `caos-eval` crate, because the `.caos-expr` walk moved there and the walk is
/// the strictest consumer of the grammar: the CLI/worker arg builder
/// ([`build_arg_entries`]), the continuation image args and the evaluator all
/// parse through the SAME [`parse_arg`], so a new type lands in one place and
/// every resolver sees it.
pub(crate) use caos_eval::{parse_arg, ArgType};

/// Pull the reserved `--base:<type>=<image>` out of a verb's arg list, returning
/// its type and value plus everything else, in order. There is no positional
/// image in any surface — CLI, worker or `.caos-expr` — so the worker an ArgTree
/// runs is named exactly like every other argument, by an operator that says how
/// to read it (design/flake-inputs.md). Exactly one `--base` is required: a verb
/// with none has nothing to run, and two is a typo worth failing on rather than
/// silently taking the last.
///
/// Every kv is parsed here (not just `base`), so a malformed argument anywhere in
/// the list is reported before we resolve or ingest anything.
pub(crate) fn split_base_arg<'a>(
    verb: &str,
    kvs: &'a [String],
) -> Result<(ArgType, &'a str, Vec<String>), String> {
    let mut base: Option<(ArgType, &str)> = None;
    let mut rest = Vec::new();
    for kv in kvs {
        let (name, ty, value) = parse_arg(kv)?;
        if name == BASE_ARG {
            if base.replace((ty, value)).is_some() {
                return Err(format!("`{verb}` given --{BASE_ARG} twice"));
            }
        } else {
            rest.push(kv.clone());
        }
    }
    let (ty, value) = base.ok_or_else(|| {
        format!("`{verb}` needs a --{BASE_ARG}:<type>=<image> arg (:@= a path, :docker= a registry ref, or :hash= an object)")
    })?;
    Ok((ty, value, rest))
}

/// Resolve a typed image ref — a `--base`, or a `map-then`'s `--map`/`--run`/
/// `--then` — into what the server runs: a git hash, or a `docker://<ref>`.
/// The TYPE decides how the value is read, never the value's shape; this is the
/// function that replaced the CLI/worker sniffers (design/flake-inputs.md, 2C).
///
/// `cas` says which world we're in, and is the only difference between the two
/// clients: `Some(dir)` is a worker, where a path names a materialized `/cas`
/// node; `None` is the CLI, where a path is a host directory to ingest (and
/// evaluate — see [`resolve_cli_image`]).
fn resolve_base(
    t: &dyn Transport,
    cas: Option<&Path>,
    ty: ArgType,
    value: &str,
) -> Result<String, String> {
    resolve_base_with_store(t, cas, ty, value, &[])
}

/// [`resolve_base`] carrying the caller's secret store through evaluation, so a
/// tool the resolved expression embeds keeps its secret-dependent identity —
/// see [`resolve_cli_image_arg`]. Only the two evaluating types read the store;
/// `:hash=` and `:docker=` name an object outright and evaluate nothing.
fn resolve_base_with_store(
    t: &dyn Transport,
    cas: Option<&Path>,
    ty: ArgType,
    value: &str,
    store: &[ClientSecret],
) -> Result<String, String> {
    match ty {
        // `:docker=<ref>` — a registry image, carried as the `docker://` ref the
        // server and `base_arg_entry` expect. The scheme is added here, so the
        // value a caller writes is the plain ref.
        ArgType::Docker => Ok(format!("{DOCKER_SCHEME}{value}")),
        // `:hash=<oid>` — a git image or a curry node already in the store (e.g.
        // what `caos curry` printed). Location-independent, so it survives being
        // passed through an arg into a worker, which a `/cas` path would not.
        ArgType::Hash => {
            if !is_hex_hash(value) {
                return Err(format!(":hash= wants an object hash, got {value:?}"));
            }
            Ok(value.to_string())
        }
        // `:@=<path>` — a `/cas` node in a worker, a host directory on the CLI.
        ArgType::Path => match cas {
            Some(cas) => resolve_cas_image(t, cas, value),
            None => resolve_cli_image_with_store(t, value, store),
        },
        // `:@@=<ref>` — the worker lives in ANOTHER repo: fetch it, then treat
        // the result exactly as a `:@=` directory, evaluating it if it carries a
        // `.caos-expr`. This is the consumer story's entry point — a project
        // names caos' `std/<x>` by locator and gets a runnable image, with only
        // the oid entering its cache key (design/flake-inputs.md).
        ArgType::Remote => {
            let (mode, oid) = resolve_remote_arg(t, value, store)?;
            if !mode.is_tree() {
                return Err(format!("git ref {value:?} names a file, not an image tree"));
            }
            Ok(oid.to_string())
        }
        ArgType::Literal => Err(format!(
            "an image needs a type: use --name:@=path, --name:@@=<git ref>, \
             --name:docker=ref or --name:hash=oid, got {value:?}"
        )),
        ArgType::Commit => Err("a commit is not an image".to_string()),
    }
}

pub use git_locator::{parse_git_ref, GitRef};

/// Resolve a `--name:@@=<ref>` argument to the `(mode, oid)` of the tree (or
/// blob) it names, fetching from another repo if that is what the locator says.
/// The oid is all that survives: URL and rev are fetch coordinates, so the arg
/// entry is byte-for-byte what a local `:@=` of the same content would produce
/// and two consumers pinning the same rev share the whole subgraph by hash
/// (design/flake-inputs.md).
///
/// Three steps, and the ORDER is the content-addressing argument: pin (the rev,
/// already validated by [`parse_git_ref`]), fetch, then select (`dir=`). We fetch
/// a COMMIT because that is what a host will serve, and descend within it —
/// rather than naming a subtree hash, which nothing would hand us.
///
/// **`dir=` names a path in the EVALUATED tree**, not the raw one: the descent
/// goes through [`eval::eval_path`], which applies every `.caos-expr` from the
/// repo root down, exactly as `eval-path` does for a local tree. That is not a
/// convenience — it is what makes an ordinary std entry reachable at all. A raw
/// walk hands the evaluator a bare `std/<x>` directory whose expression names
/// `DEEP-DEPS/<dep>` mounts that only exist once the ROOT expression has
/// deepened the tree, so it fails with `base path "DEEP-DEPS/…" not found in
/// tree`; and a seeded entry like `std/rustc` forms its key from its *deepened*
/// entry, which a raw fetch cannot reproduce. Descending through evaluation
/// makes a pinned consumer see caos exactly as caos sees itself.
fn resolve_remote_arg(
    t: &dyn Transport,
    value: &str,
    store: &[ClientSecret],
) -> Result<(gix::objs::tree::EntryMode, gix::ObjectId), String> {
    let git_ref = parse_git_ref(value)?;

    // MEMOIZED FOR THE FETCH SCHEMES ONLY, and the split is the pin: a `git+` /
    // `github:` locator carries a mandatory full commit sha, so the whole
    // resolution — fetch, peel, descend — is immutable and the locator string is
    // a content key like any other. A `path:` locator reads a LIVE directory,
    // whose bytes change under an editor, so it is re-ingested every time.
    //
    // Worth its own memo on top of `eval_path`'s: a repeat here also skips the
    // `git cat-file` that `fetch_git_ref` probes with and the commit peel, so a
    // consumer naming one pin twice touches git once.
    let memo_key = (!git_ref.is_plain_dir()).then(|| format!("{}\u{0}{value}", store_key(store)));
    if let Some(key) = &memo_key {
        if let Some(hit) = REMOTE_ARG_MEMO.get(key) {
            return Ok(hit);
        }
    }

    // `path:` — a live local directory, hashed now, exactly like a `:@=` path
    // (so "only what git tracks is visible" still holds). No rev: there is
    // nothing to pin, because there is no fetch.
    let root = if git_ref.is_plain_dir() {
        let dir = git_ref
            .url
            .strip_prefix("path:")
            .expect("is_plain_dir checked the prefix");
        let (mode, oid) = t.ingest_path(dir)?.ok_or_else(|| {
            format!("`:@@=path:` reads a host directory, which this client cannot do ({dir})")
        })?;
        // A file has nothing to descend into and no expression to apply.
        if !mode.is_tree() {
            if let Some(dir) = &git_ref.dir {
                return Err(format!(
                    "git ref {value:?}: `dir={dir}` but {} names a file",
                    git_ref.url
                ));
            }
            return Ok((mode, oid));
        }
        oid
    } else {
        let rev = git_ref
            .rev
            .as_deref()
            .expect("parse_git_ref requires a rev for every fetch scheme");
        let url = git_ref.fetch_url();
        t.fetch_git_ref(&url, rev)?.ok_or_else(|| {
            format!(
                "cannot fetch {value:?}: resolving a remote ref is a CLIENT capability; \\
                 a remote locator must already be an oid by the time a worker sees it"
            )
        })?
    };

    let dir = git_ref.dir.as_deref().unwrap_or("");
    // Evaluate SERVER-SIDE, not with a local `.caos-expr` walk. The walk is
    // CHATTY -- measured ~54 round trips -- which is 0.1s over a loopback and
    // costs whatever the link costs otherwise. `/eval-locator` does the whole
    // walk inside the server, where each hop is sub-millisecond, so this makes
    // ONE request. The result is byte-identical to `eval_path` (secret marking
    // is a no-op through eval; it happens when the step RUNS).
    //
    // WORTH MORE THAN IT LOOKS, and worth keeping even though the transport it
    // was written for is gone. Over the dumbpipe tunnel this replaced, those 54
    // round trips were 54 fresh CONNECTIONS — dumbpipe dials the far endpoint
    // per accepted socket — and took ~30s. `caos://` costs a stream rather than
    // a handshake and reaches the server directly
    // (design/iroh-transport.md), so the same walk is now well under a second;
    // one request still beats 54, and over a genuinely distant server the
    // difference is the round trips, which nothing local can remove.
    //
    // Falls back to the local walk on any failure: an older server without the
    // endpoint, or a real eval error, both land here, and the local walk then
    // either succeeds (slowly) or reproduces the same error to report.
    let (kind, hash) = eval_locator_on_server(t, &root.to_string(), dir, store)
        .or_else(|_| eval::eval_path(t, &root.to_string(), dir, store))
        .map_err(|e| format!("git ref {value:?}: {e}"))?;
    let resolved = (eval::mode_of_kind(&kind), parse_oid(&hash)?);
    if let Some(key) = memo_key {
        REMOTE_ARG_MEMO.put(key, resolved);
    }
    Ok(resolved)
}

/// [`resolve_remote_arg`]'s memo: `<store>\0<locator>` → `(mode, oid)`, for the
/// pinned schemes only. See the split at the top of that function.
static REMOTE_ARG_MEMO: eval::Memo<(gix::objs::tree::EntryMode, gix::ObjectId)> = eval::Memo::new();

/// Evaluate `dir` within the tree `root` on the SERVER, via `/eval-locator` --
/// [`resolve_remote_arg`]'s fast path. Pushes `root` so the server can read it,
/// then makes ONE request in place of the local walk's ~54. Returns
/// `(kind, hash)`, exactly as [`eval::eval_path`] does, so the two are
/// interchangeable and the caller can fall back to the walk on any failure.
fn eval_locator_on_server(
    t: &dyn Transport,
    root: &str,
    dir: &str,
    store: &[ClientSecret],
) -> Result<(String, String), String> {
    // The server percent-decodes a query value and splits on `&`; a `#` ends the
    // query. A path carrying any of those (or a `%`) is left to the local walk.
    if dir.chars().any(|c| matches!(c, '&' | '#' | '%')) {
        return Err(format!("eval dir {dir:?} has a query-unsafe character"));
    }
    t.ensure_pushed(root)?;
    request_compute_url(
        &t.server_url()?,
        &format!("/eval-locator?in={root}&eval={dir}"),
        &secret_store_header(store),
    )
}

/// Resolve curry layers, build the args tree, bundle + push the request, and run
/// it — the CLI's blocking run. Ordinary worker sub-runs are continuations the
/// server resolves; detached worker work uses `sub-run` to retain the current
/// server-side run context. Returns the server's
/// `(kind, result-hash)`. `cas` is `None` here: every path arg is a host path to
/// ingest.
fn run_request(
    t: &dyn Transport,
    image: &str,
    cas: Option<&Path>,
    kvs: &[String],
    store: &[ClientSecret],
) -> Result<(String, String), String> {
    let arg_tree = prepare_request(t, image, cas, kvs, store)?;
    let header = secret_store_header(store);
    // Trigger compute; the server runs the container and returns the result's
    // "<type> <hash>" (and, for a top-level run, pins refs/caos/res/<argTreeHash>
    // at it).
    let server = t.server_url()?;
    // Watch the work while the compute request blocks. Both `run` and
    // `run-tool` come through here, so the live display is one place rather
    // than two, and a caller that is not a person (the suite, a worker) gets
    // nothing started on its behalf — see `watch::Watch::start`.
    let _watch = watch::Watch::start(&server, &arg_tree);
    request_compute(&server, &arg_tree, &header)
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

/// Build and push a host-side request with scalar/commit arguments and no
/// secret store. Higher-level clients can durably record the returned request
/// id before dispatching it.
pub fn prepare_client_request(
    t: &dyn Transport,
    image: &str,
    kvs: &[String],
) -> Result<String, String> {
    prepare_request(t, image, None, kvs, &[])
}

/// Build and push a host-side request while folding the supplied local secret
/// store's identities into the ArgTree. Secret values remain out of band.
pub fn prepare_client_request_with_store(
    t: &dyn Transport,
    image: &str,
    kvs: &[String],
    store: &[ClientSecret],
) -> Result<String, String> {
    prepare_request(t, image, None, kvs, store)
}

/// Prepare and synchronously run one host-side request with an already-resolved
/// local secret store. This is the non-streaming client equivalent of
/// [`cli_run`], for callers that need the result identity rather than CLI output
/// handling. Durable conversation turns deliberately use the split prepare and
/// compute APIs instead so they can record the request before dispatching it.
pub fn run_client_request_with_store(
    t: &dyn Transport,
    image: &str,
    kvs: &[String],
    store: &[ClientSecret],
) -> Result<(String, String), String> {
    run_request(t, image, None, kvs, store)
}

/// `prepare-request --base:<type>=<image-or-arg-tree> [--name=value | --name:@=path ...]`
/// — construct the exact flat runnable ArgTree and print its hash without
/// executing it. This is the worker-side half: CAS paths use `/cas` semantics.
///
/// Unlike [`caos_curry`], the result is not a partial curry node. It is the same
/// request `run_request` would send to `/run`, so it can be recorded durably
/// and later handed unchanged to `sub-run` or `run-request-then`.
pub fn caos_prepare_request(t: &dyn Transport, kvs: &[String]) -> Result<(), String> {
    let cas = cas_dir();
    let (bty, bval, kvs) = split_base_arg("prepare-request", kvs)?;
    let image = resolve_base(t, Some(&cas), bty, bval)?;
    println!("{}", prepare_request(t, &image, Some(&cas), &kvs, &[])?);
    Ok(())
}

/// User-facing [`caos_prepare_request`]. Host paths are ingested with the same
/// semantics as [`cli_run`], and the flat request is pushed before its hash is
/// printed so another process can immediately run it.
pub fn cli_prepare_request(t: &dyn Transport, kvs: &[String]) -> Result<(), String> {
    let (bty, bval, kvs) = split_base_arg("prepare-request", kvs)?;
    let image = resolve_base(t, None, bty, bval)?;
    let store = build_secret_store(t)?;
    println!("{}", prepare_request(t, &image, None, &kvs, &store)?);
    Ok(())
}

/// Assemble a runnable ArgTree from a base image ref and the caller's already
/// resolved `call` args, folding in the reserved `base`/`salt`/`std` entries,
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

    // The worker (image) rides *in* the args tree under the reserved `base`
    // entry, rather than as a sibling of `args` in the request. So a computation
    // is identified entirely by its args (an executor can match on the worker
    // alongside the rest), and a worker — which sees its args at `/cas/args` —
    // reaches its own image at `/cas/args/base` to call itself. Merged last so
    // the reserved name wins over any like-named user arg.
    //
    // A git-docker image *is* a git tree, so we reference it by that tree (the
    // entry's oid is the image tree): the image then travels inside the request's
    // own object graph — no separate push — and materializes at `/cas/args/base`
    // as a real directory whose recorded hash is the image, so recursion can pass
    // that path straight to `caos run`. A `docker://` ref has no git object to
    // embed, so it rides as a blob naming the registry ref.
    let image_entry = base_arg_entry(t, &image)?;
    let mut arg_entries = merge_entries(merge_entries(bound, call), vec![image_entry]);

    // The cache-busting salt (empty by default) rides *in* the args tree under the
    // reserved `salt` entry, exactly like `base` — per SPEC an ArgTree is a git
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

/// `map-then <in> [--map:<type>=<image>] [--then:<type>=<image>]` — the
/// *worker* form: record a continuation `{in, map?, then?}` as this worker's result at
/// `/cas/out`, fetching and running nothing. The worker then exits, and the
/// *server* resolves the continuation — `map` over each child of `in` in
/// parallel, then `then(--in, --children)` — with no worker slot held (see
/// `design/map-then.md`). So `caos map-then` is a tail call: it produces `/cas/out`
/// itself and must be the worker's final act. At least one of `--map`/`--then`
/// is required; each names an ArgTree, TYPED like a `--base` — `:@=` a `/cas`
/// path, `:docker=` a registry ref, `:hash=` an object already in the store —
/// and resolved through the same path a `--base` takes (`resolve_base`).
/// (The user-facing CLI's blocking run is [`cli_run`]; the single-valued form
/// is [`caos_run_then`].)
///
/// `--max-parallel=<n>` bounds how many children are IN FLIGHT at once; absent,
/// all of them are, which is what this always did. It is the only way to bound a
/// fan-out, because the runner pool bounds CONTAINERS and a child that has
/// recorded a continuation and exited holds no container while the work it is
/// waiting for runs — so a 46-way map reaches 46 children in flight however few
/// runner slots exist. The server holds one thread per in-flight child, spanning
/// that child's whole chain, which is exactly the quantity being bounded.
pub fn caos_map_then(t: &dyn Transport, input: &str, kvs: &[String]) -> Result<(), String> {
    // The WIDTH is checked here, where the caller can see it. The server checks
    // it too, but a continuation is resolved long after the worker that recorded
    // it has exited — so a bad width discovered there is a failure with nobody
    // left to tell. Zero is the one worth naming: it reads as "no parallelism"
    // and would mean "no child ever runs".
    for kv in kvs {
        if let Some(value) = kv.strip_prefix("--max-parallel=") {
            match value.parse::<usize>() {
                Ok(n) if n >= 1 => {}
                _ => {
                    return Err(format!(
                        "`map-then --max-parallel` wants a positive integer, got {value:?}"
                    ))
                }
            }
        }
    }
    record_continuation(
        t,
        "map-then",
        ContinuationSubject::Input(input),
        kvs,
        &["map", "then"],
        // `max-parallel` is a LITERAL, recorded verbatim: it is a count, not an
        // image to resolve.
        &["max-parallel"],
        &[],
        |given| {
            if given.is_empty() {
                return Err("`map-then` needs --map and/or --then".to_string());
            }
            if given.contains(&"max-parallel") && !given.contains(&"map") {
                return Err(
                    "`map-then --max-parallel` needs --map: it bounds the fan-out, and \
                     without a --map there is nothing to fan out"
                        .to_string(),
                );
            }
            Ok(())
        },
    )
}

/// `run-then <in> --run:<type>=<image> [--then:<type>=<image>] [--catch]` — the
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
        ContinuationSubject::Input(input),
        kvs,
        &["run", "then"],
        &[],
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

/// `eval-path-then <in> --eval=<path> [--then:<type>=<image>] [--catch]` — the
/// evaluation sibling of [`caos_run_then`]: record a continuation `{in, eval,
/// then?, catch?}` and exit. The SERVER walks `.caos-expr` from `in`'s root down
/// to `<path>` (blocking a request thread, its own `run`s dispatched normally —
/// design/caos-expr.md), yielding R; with `--then` the result is
/// `then(--in=<in>, --result=<R>)`, else R itself. `--eval` names a PATH within
/// `in` (recorded verbatim, not an image); `--catch` turns a failed walk into
/// `--error` (and needs `--then`), like run-then's. This is how a WORKER — which
/// may not block on a run — gets a `.caos-expr` evaluated: it asks the server to.
pub fn caos_eval_then(t: &dyn Transport, input: &str, kvs: &[String]) -> Result<(), String> {
    record_continuation(
        t,
        "eval-path-then",
        ContinuationSubject::Input(input),
        kvs,
        &["then"],
        &["eval"],
        &["catch"],
        |given| {
            if !given.contains(&"eval") {
                return Err("`eval-path-then` needs --eval=<path>".to_string());
            }
            if given.contains(&"catch") && !given.contains(&"then") {
                return Err(
                    "`eval-path-then --catch` needs --then: the error has to be delivered somewhere"
                        .to_string(),
                );
            }
            Ok(())
        },
    )
}

/// `run-request-then <R> [--then:<type>=<image>] [--catch]` — record a promise
/// that runs the already-complete ArgTree `R` unchanged. With `--then`, its
/// result is passed as that callback's sole `--result` arg; without one, R's
/// result is this request's result. `--catch` instead passes a failed R as the
/// callback's sole `--error` arg and therefore requires `--then`.
///
/// `R` may be a 40-character tree hash already stored on the server or a tree
/// path inside `/cas`. Unlike `run-then`, no `--in` is added and no new request
/// is assembled around an image: R's hash is the request identity executed by
/// the promise interpreter.
pub fn caos_run_request_then(
    t: &dyn Transport,
    request: &str,
    kvs: &[String],
) -> Result<(), String> {
    record_continuation(
        t,
        "run-request-then",
        ContinuationSubject::Request(request),
        kvs,
        &["then"],
        &[],
        &["catch"],
        |given| {
            if given.contains(&"catch") && !given.contains(&"then") {
                return Err(
                    "`run-request-then --catch` needs --then: the error has to be delivered somewhere"
                        .to_string(),
                );
            }
            Ok(())
        },
    )
}

/// Start an already-stored ArgTree in the current job's server-side run context
/// without waiting for its result. The job nonce identifies that context; the
/// worker never receives the carried stack or secret store.
pub fn caos_sub_run(t: &dyn Transport, arg_tree: &str) -> Result<(), String> {
    if !is_hex_hash(arg_tree) || arg_tree.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(format!(
            "sub-run needs a lowercase 40-character ArgTree hash, got {arg_tree:?}"
        ));
    }
    if !t.has_object(arg_tree)? {
        return Err(format!(
            "sub-run needs an already-stored ArgTree, and {arg_tree} is absent"
        ));
    }
    let (kind, content) = t.get_object(arg_tree)?;
    if kind != "tree" {
        return Err(format!(
            "sub-run needs an ArgTree, but {arg_tree} is a {kind}"
        ));
    }
    let tree = gix::objs::TreeRef::from_bytes(&content, gix::hash::Kind::Sha1)
        .map_err(|error| format!("sub-run ArgTree {arg_tree} is malformed: {error}"))?;
    if !tree
        .entries
        .iter()
        .any(|entry| entry.filename.to_vec().as_slice() == b"base")
    {
        return Err(format!(
            "sub-run needs a runnable ArgTree, but {arg_tree} has no 'base' entry"
        ));
    }
    t.ensure_pushed(arg_tree)?;
    let nonce = std::env::var(JOB_NONCE_ENV)
        .map_err(|_| "sub-run is available only inside a running worker".to_string())?;
    request_sub_run(&t.server_url()?, arg_tree, &nonce)?;
    println!("request {arg_tree}");
    Ok(())
}

/// `trace-child <name> <arg-tree>` — record, under THIS job's trace record, that
/// it started `arg-tree` under `name`.
///
/// For work a job starts on ANOTHER STACK. A dev stack brought up inside a
/// worker writes its trace records to the same redis the host uses, and a trace
/// key carries no cache namespace, so the two sets of records already sit side
/// by side — the only thing missing is the edge that joins them. With it,
/// `caos-cli status` on the outer job descends into everything the inner stack
/// did, which is what makes a long `run-tool test` watchable rather than opaque.
///
/// It records an EDGE and nothing else: the child's own record is written by
/// whichever server ran it. So this is safe to call before the work starts —
/// and it has to be, since the point is to watch it while it runs.
pub fn caos_trace_child(t: &dyn Transport, name: &str, arg_tree: &str) -> Result<(), String> {
    if arg_tree.len() != 40 || !arg_tree.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "trace-child needs a 40-character ArgTree hash, got {arg_tree:?}"
        ));
    }
    let nonce = std::env::var(JOB_NONCE_ENV)
        .map_err(|_| "trace-child is available only inside a running worker".to_string())?;
    let body = serde_json::json!({"req": arg_tree, "nonce": nonce, "name": name}).to_string();
    server_call(
        &t.server_url()?,
        &ServerRequest {
            method: "POST",
            path: "/trace/child",
            headers: &[("content-type", "application/json".to_string())],
            body: Some(body.as_bytes()),
            timeout_secs: Some(5),
        },
    )?;
    Ok(())
}

enum ContinuationSubject<'a> {
    Input(&'a str),
    Request(&'a str),
}

/// Shared body of [`caos_map_then`] / [`caos_run_then`] / [`caos_eval_then`] /
/// [`caos_run_request_then`]: record a continuation over `subject` — either the
/// ordinary `in` data entry or an exact `request` ArgTree — as this worker's
/// result at `/cas/out` (a `promise` placeholder the server resolves once the
/// job is posted). `allowed` names the image-valued entries this verb accepts
/// (each resolved to a hash), `literals` names entries whose value is recorded
/// VERBATIM as a blob (e.g. `eval`'s path — a string, not an image), `markers`
/// names its bare flags (recorded as one-byte blobs; the interpreter reads only
/// their presence), and `check` validates the set actually given, before
/// anything is sealed.
// Three kinds of key (image / literal / marker) plus the fixed t/verb/subject/kvs
// and the validator — one over clippy's arg limit, and splitting the key kinds
// into a struct would only move the noise to the call sites.
#[allow(clippy::too_many_arguments)]
fn record_continuation(
    t: &dyn Transport,
    verb: &str,
    subject: ContinuationSubject<'_>,
    kvs: &[String],
    allowed: &[&'static str],
    literals: &[&'static str],
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

    let subject = match subject {
        // `in` is the data node the continuation is over: an existing CAS path,
        // referenced as a real tree entry (mode + recorded hash).
        ContinuationSubject::Input(input) => {
            let path = validate_descendant(&cas, input)?;
            let (mode, oid) = cas_entry(&path)?;
            Entry {
                mode,
                filename: b"in".to_vec().into(),
                oid,
            }
        }
        // `request` is a complete ArgTree. Store it as a tree entry so the
        // continuation names R directly rather than a blob that must be
        // interpreted and rebuilt.
        ContinuationSubject::Request(request) => {
            let (mode, oid) = if Path::new(request).starts_with(&cas) {
                let path = validate_descendant(&cas, request)?;
                cas_entry(&path)?
            } else {
                if !is_hex_hash(request) {
                    return Err(format!(
                        "`run-request-then` needs a 40-character ArgTree hash or /cas tree path, got {request:?}"
                    ));
                }
                let (kind, _) = t.get_object(request)?;
                if kind != "tree" {
                    return Err(format!("request {request} is a {kind}, not a tree"));
                }
                (EntryKind::Tree.into(), parse_oid(request)?)
            };
            if !mode.is_tree() {
                return Err(format!("request {request:?} is not a tree"));
            }
            t.ensure_pushed(&oid.to_string())?;
            Entry {
                mode: EntryKind::Tree.into(),
                filename: b"request".to_vec().into(),
                oid,
            }
        }
    };
    let mut entries = vec![subject];

    let mut given: Vec<&str> = Vec::new();
    for kv in kvs {
        // Markers are bare flags, matched BEFORE parse_arg — which requires a
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
        // A LITERAL entry (e.g. `eval`'s path): its value is recorded verbatim as
        // a blob, not resolved as an image.
        if let Some(&name) = literals.iter().find(|&&l| l == name) {
            if given.contains(&name) {
                return Err(format!("--{name} given twice"));
            }
            if matches!(ty, ArgType::Commit) {
                return Err(format!("--{name} is a path/value, not a commit"));
            }
            entries.push(Entry {
                mode: EntryKind::Blob.into(),
                filename: name.as_bytes().to_vec().into(),
                oid: post_object(t, "blob", value.as_bytes())?,
            });
            given.push(name);
            continue;
        }
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
        // Each of these flags names an ArgTree to run, typed exactly like a
        // `--base`: `:@=` a `/cas` path, `:docker=` a registry ref, `:hash=` an
        // object already in the store (typically what `caos curry` printed).
        let resolved =
            resolve_base(t, Some(&cas), ty, value).map_err(|e| format!("--{name}: {e}"))?;
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

/// `run [output] --base:<type>=<image> [--name=value | --name:@=path ...]`
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
/// the worker is the reserved [`BASE_ARG`] — `--base:@=<host dir>` (ingested, and
/// evaluated if it carries a `.caos-expr`; see [`resolve_cli_image`]),
/// `--base:docker=<ref>`, or `--base:hash=<oid>`.
pub fn cli_run(t: &dyn Transport, output: Option<&str>, kvs: &[String]) -> Result<(), String> {
    let (bty, bval, kvs) = split_base_arg("run", kvs)?;
    let image = resolve_base(t, None, bty, bval)?;
    // Build the ephemeral secrets store from the caller's `.caos-secrets`
    // (design/secrets.md), resolving each reader here — where eval-path is
    // available — so the server never evals. Empty when there's no store.
    let store = build_secret_store(t)?;
    let (kind, result) = run_request(t, &image, None, &kvs, &store)?;

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

/// The reserved `base` entry for an args tree, carrying the worker image `image`
/// (a resolved ref: `docker://…` or a git-image hash). A git-docker image *is* a
/// git tree, so it rides embedded — the entry references that tree directly, so
/// the image travels inside the request's object graph and materializes as a real
/// directory at `/cas/args/base`. A `docker://` ref has no git object to embed,
/// so it rides as a blob naming the registry ref.
fn base_arg_entry(t: &dyn Transport, image: &str) -> Result<gix::objs::tree::Entry, String> {
    use gix::objs::tree::{Entry, EntryKind};
    let (mode, oid) = if is_hex_hash(image) {
        (EntryKind::Tree, parse_oid(image)?)
    } else {
        (EntryKind::Blob, post_object(t, "blob", image.as_bytes())?)
    };
    Ok(Entry {
        mode: mode.into(),
        filename: b"base".to_vec().into(),
        oid,
    })
}

/// Build the args tree's reserved `salt` entry: the cache-busting salt as a plain
/// blob. The counterpart of [`base_arg_entry`] for the other
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

/// Resolve a worker-side `:@=` image path — a node under the CAS — to what the
/// server expects: the git hash recorded on it, or, for a node whose *content*
/// is a `docker://` ref, that ref.
///
/// Reading the content is not sniffing a caller's token: the path was typed
/// `:@=` by the operator, and what's found there is an object caos itself
/// recorded, exactly as [`base_arg_entry`] re-derives an entry from a stored
/// ref. A path outside the CAS is rejected — a worker has no host filesystem.
fn resolve_cas_image(t: &dyn Transport, cas: &Path, image: &str) -> Result<String, String> {
    if !Path::new(image).starts_with(cas) {
        return Err(format!(
            "an image path must be under {} (a worker has no host filesystem), got: {image}",
            cas.display()
        ));
    }
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
    read_hash(&canon)
}

/// Resolve a CLI-side `:@=` image path — a host DIRECTORY — by ingesting it and
/// evaluating it, which is the only image form the CLI reads off the filesystem
/// (`:docker=` and `:hash=` name things that need no host at all).
///
/// A path is the only name a caller needs, because a tree says how it is built.
/// There is deliberately no name-to-image lookup here: the CLI resolves a
/// dependency by DESCENT through the tree it was handed (`DEEP-DEPS/<name>`),
/// which is what makes a caller's dependencies its own declared edges rather
/// than whatever an ambient library happens to hold.
///
/// The descent starts at the SOURCE_TREE ROOT, not at the named directory. It used
/// to ingest only that directory and evaluate it in isolation, which quietly made
/// the local operator weaker than the remote one:
///
/// ```text
/// $ caos-cli eval-path std/hello            # descends from the root
/// tree e72a747…
/// $ caos-cli run --base:@=std/hello         # evaluated in isolation
/// eval-path: base path "DEEP-DEPS/rustc" not found in tree
/// ```
///
/// Both failed for the same reason a raw `:@@=` walk did (design/flake-inputs.md,
/// 4a): an entry's expression names `DEEP-DEPS/<dep>` mounts that exist only once
/// the ROOT expression has deepened the tree, so evaluating the entry alone can
/// never see them — a pinned consumer could reach `std/hello` while this repo
/// could not reach its own.
///
/// Two consequences. The path need not exist ON DISK, since `DEEP-DEPS/<name>`
/// is created by the root expression; and in a repo carrying a root `.caos-expr`
/// a `:@=` image deepens the whole tree first — a cached run, and exactly what
/// `eval-path` and `run-tool` already do.
pub fn resolve_cli_image(t: &dyn Transport, image: &str) -> Result<String, String> {
    resolve_cli_image_with_store(t, image, &[])
}

/// Resolve one `--<name>:<type>=<value>` image argument as a CLIENT reads it —
/// the same four spellings `run` and `curry` take (`:@=` a workspace path,
/// `:@@=` a git locator, `:hash=` an oid, `:docker=` a registry ref), against
/// the caller's secret store.
///
/// This is how a client command names a TOOL it needs — `caos tui
/// --llm-step:@=caos-std/llm-step` — instead of descending a path it decided
/// on. The convention was `DEEP-DEPS/<name>`, expanded from a root `DEPS`,
/// which obliged every repo driving this client to declare caos' entry points
/// under the names the client happened to use. Naming the image in the
/// invocation moves that choice to the caller, and `:@@=` lets a repo that
/// never mounted caos reach a tool at all.
pub fn resolve_cli_image_arg(
    t: &dyn Transport,
    argument: &str,
    store: &[ClientSecret],
) -> Result<String, String> {
    let (_, ty, value) = parse_arg(argument)?;
    resolve_base_with_store(t, None, ty, value, store)
}

/// [`resolve_cli_image_arg`] with a `:@=` path looked up in `tree` instead of in
/// the working directory.
///
/// WHICH TREE A CLIENT'S TOOLS COME FROM, and for anything that RECORDS a
/// conversation the working directory is the wrong answer: the conversation's
/// content is its base commit, so a step resolved from the worktree can come
/// from a different tree than the session records. That gap is not theoretical —
/// it is what obliged a dev-mode cloud session to rewrite `.caos-expr` and
/// `flake.lock` ON DISK, leaving a checkout dirty with a `caos://` ticket
/// (a credential) for an agent to be asked to commit. Naming the tree keeps the
/// two together by construction, and the rewrite then lives only in the
/// unreferenced commit the conversation seeds from.
///
/// The other three types are unaffected and delegate: they name an object
/// outright or fetch it, so the working directory never entered them.
pub fn resolve_cli_image_arg_in_tree(
    t: &dyn Transport,
    tree: &str,
    argument: &str,
    store: &[ClientSecret],
) -> Result<String, String> {
    let (_, ty, value) = parse_arg(argument)?;
    match ty {
        ArgType::Path => eval::eval_path(t, tree, value, store)
            .map(|(_kind, hash)| hash)
            .map_err(|error| format!("resolving {value:?} in tree {tree}: {error}")),
        _ => resolve_base_with_store(t, None, ty, value, store),
    }
}

/// [`resolve_cli_image`] carrying the caller's secret store into the walk, so a
/// `run` the expression dispatches and any `curry` it returns are marked with
/// the caller's identity (design/secrets.md). Conversation setup uses this
/// form: the step it resolves embeds tools whose arg trees have to match the
/// readers granting the model key, and an unmarked resolution would not.
pub fn resolve_cli_image_with_store(
    t: &dyn Transport,
    image: &str,
    store: &[ClientSecret],
) -> Result<String, String> {
    // The tracked workspace (dirty edits included), exactly as `eval-path` with
    // no `--tree` starts. A flake dir is NOT special-cased here or on the
    // server — it carries a `.caos-expr` naming its builder, and the evaluation
    // turns it into an image, so what the server receives is already one
    // (design/caos-expr.md).
    let (_, ws) = t
        .ingest_path(".")?
        .ok_or_else(|| "this client cannot ingest the source tree".to_string())?;
    // Descend THROUGH evaluation: each `.caos-expr` from the root down is
    // applied, and `image` is looked up in what the one above it produced. A
    // tree with no `.caos-expr` (a plain flake dir, a git-docker image)
    // evaluates to itself and nothing changes.
    eval::eval_path(t, &ws.to_string(), image, store)
        .map(|(_kind, hash)| hash)
        .map_err(|e| format!("resolving {image:?}: {e}"))
}

/// `curry [--unbind=<name> …] --base:<type>=<arg tree> [--name=value ...]` —
/// bind arguments to the `--base` arg tree, printing a ref (a git hash) to the new
/// arg tree that includes
/// the new args. The ref can be `run` — which supplies the rest of the args —
/// or `curry`'d again, exactly like any other arg tree; the binding is partial
/// application, not a rebuilt container image. This is the *worker* form: path
/// args resolve against `/cas`. (The CLI's is [`cli_curry`].)
///
/// Currying is an ArgTree → ArgTree operation. The `--base` may be given in any of
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
pub fn caos_curry(t: &dyn Transport, rest: &[String]) -> Result<(), String> {
    let cas = cas_dir();
    let (unbind, kvs) = split_curry_args(rest);
    let (bty, bval, kvs) = split_base_arg("curry", &kvs)?;
    let arg_tree = resolve_base(t, Some(&cas), bty, bval)?;
    println!("{}", curry_object(t, &arg_tree, Some(&cas), &unbind, &kvs)?);
    Ok(())
}

/// `curry [--unbind=<name> …] --base:<type>=<arg tree> [--name=value ...]` —
/// the *CLI* form of [`caos_curry`]: a `--base:@=` is a host directory to ingest
/// and evaluate, path args are host paths to ingest, and the curried
/// arg tree is pushed so a later `run` can use the printed ref directly.
pub fn cli_curry(t: &dyn Transport, rest: &[String]) -> Result<(), String> {
    let (unbind, kvs) = split_curry_args(rest);
    let (bty, bval, kvs) = split_base_arg("curry", &kvs)?;
    let arg_tree = resolve_base(t, None, bty, bval)?;
    let curried = curry_object(t, &arg_tree, None, &unbind, &kvs)?;
    t.ensure_pushed(&curried.to_string())?;
    if is_hex_hash(&arg_tree) {
        t.ensure_pushed(&arg_tree)?;
    }
    println!("{curried}");
    Ok(())
}

/// Split a `curry`'s args — `[--unbind=<name> …] --base:<type>=<arg tree>
/// [--name=value …]` — into the unbind names and everything else.
///
/// There is no `--` separator anywhere in the grammar: what keeps the verb's own
/// operands apart from the args it binds is that their NAMES are reserved.
/// `unbind` is one (repeatable), [`BASE_ARG`] the other, so neither can be bound
/// as an ordinary arg — the same rule, applied uniformly, that lets `run` take
/// its worker as `--base` instead of a positional (design/flake-inputs.md).
/// Order is therefore free: an `--unbind=` may sit anywhere among the binds.
fn split_curry_args(rest: &[String]) -> (Vec<&str>, Vec<String>) {
    let mut unbind = Vec::new();
    let mut kvs = Vec::new();
    for a in rest {
        match a.strip_prefix("--unbind=") {
            Some(name) => unbind.push(name),
            None => kvs.push(a.clone()),
        }
    }
    (unbind, kvs)
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

/// Bind host-side scalar/commit arguments to an existing ArgTree.
pub fn curry_client_object(
    t: &dyn Transport,
    arg_tree: &str,
    kvs: &[String],
) -> Result<gix::ObjectId, String> {
    curry_object(t, arg_tree, None, &[], kvs)
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

/// If `hash` names a flat **args tree** — a tree carrying the reserved `base`
/// entry but no [`CURRY_MARKER`] — return its base image ref (from the `base`
/// entry: a git image's tree oid, or a `docker://` blob's contents) and its
/// remaining entries as bound args. This is the shape the server materializes at
/// `/cas/args` (hence what `own_args_tree` names); `None` for a curry node, a
/// plain image, or any tree without a `base` entry.
///
/// `base` ALONE DOES NOT SAY "args tree": a git-docker image tree carries its
/// own `base` — the `docker://` ref its `layer<NN>`s are a delta over (SPEC,
/// "Git-tree image"). Reading one as an args tree peels it into its own base and
/// scatters `config.json`/`layer<NN>` into the caller's args, so the run goes to
/// the raw registry ref instead of the converted image, and `run-tool test` dies
/// at `lookup caos-registry ... no such host` — the SERVER's name for the
/// registry, which the host daemon cannot resolve. `config.json` is the
/// discriminator: the converter requires it on every image tree, and it can
/// never be an arg name (arg names are `[a-z][a-z0-9-]*` — no dot).
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
    if entries.iter().any(|e| entry_name(e) == b"config.json") {
        return Ok(None); // a git-docker image tree, whose `base` is its own
    }
    let Some(image) = entries.iter().find(|e| entry_name(e) == b"base") else {
        return Ok(None); // no reserved `base` entry — not an args tree
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
        .filter(|e| entry_name(e) != b"base")
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
/// Public so clients that *write* the store (the tui's first-run key setup)
/// name the same directory the loader reads.
pub const SECRETS_DIR: &str = ".caos-secrets";

/// Minimum entropy length (chars) not flagged weak. A `secret-hash` is only
/// unguessable if the entropy is: below this, it's brute-forceable out of the
/// hash (like GitHub Actions refusing to mask short secrets).
const MIN_ENTROPY_LEN: usize = 16;

/// Fresh entropy for a secret: 128 random bits as 32 hex chars. Public so a
/// client writing a new secret file can include the entropy up front (what
/// `secrets` would otherwise add on a second pass) with the one policy.
pub fn fresh_entropy() -> Result<String, String> {
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

    let mut issues = 0;
    let mut env_entropy_missing = false;
    for (name, path) in local_secret_files(dir)? {
        let text = std::fs::read_to_string(&path).map_err(|e| format!("reading {name}: {e}"))?;
        let spec = parse_local_secret_spec(&name, &text)?;
        // `entropy:env=` is already specified, so there is nothing to fill in
        // and nothing to write. Its LENGTH is still worth checking when the
        // variable happens to be set here, but its absence is not an issue:
        // this command is an offline lint that a CI gate runs, and CI is
        // exactly where a user's entropy variable is not going to be set.
        if let Some(LocalEntropy::Env(var)) = &spec.entropy {
            if let Some(value) = non_empty_env(var) {
                if value.len() < MIN_ENTROPY_LEN {
                    eprintln!(
                        "{name}: weak entropy in ${var} ({} chars < {MIN_ENTROPY_LEN})",
                        value.len()
                    );
                    issues += 1;
                }
            }
            continue;
        }
        match spec.entropy.as_ref().map(|e| match e {
            LocalEntropy::Literal(literal) => literal.as_str(),
            LocalEntropy::Env(var) => var.as_str(),
        }) {
            // A `value:env=` file is the COMMITTED form, so the entropy this
            // would otherwise generate must not be written into it — and this
            // command cannot pick the variable to read it from either. Say what
            // to add instead of writing something the loader will then refuse.
            None if matches!(spec.value, Some(LocalSecretValue::Env(_))) => {
                eprintln!(
                    "{name}: value:env= with no entropy — add `entropy:env=<VAR>` \
                     (a committed file cannot carry a literal one)"
                );
                issues += 1;
                env_entropy_missing = true;
            }
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
    // A `value:env=` file with no entropy is an error whether or not `--check`
    // was asked for: nothing was written to fix it, so returning success would
    // report a store this command has just declined to repair as tended.
    if issues > 0 && (check || env_entropy_missing) {
        return Err(format!("{issues} secret(s) with missing or weak entropy"));
    }
    if !check && issues > 0 {
        eprintln!("warning: {issues} secret(s) have weak entropy (edit them; not overwritten)");
    }
    Ok(())
}

/// A resolved secret from the caller's store: its value, its entropy (the
/// cache-isolation capability), and each reader resolved to a partial arg tree.
pub struct ClientSecret {
    name: String,
    value: String,
    entropy: String,
    readers: Vec<std::collections::BTreeMap<String, String>>,
}

impl ClientSecret {
    /// Worker-visible name used for `/secret/<name>`.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Check local configuration without evaluating reader expressions or building
/// worker images. Grants are resolved when preparing an actual request.
pub fn local_secret_present(dir: &Path, name: &str) -> Result<bool, String> {
    if !dir.is_dir() {
        return Ok(false);
    }
    let mut present = false;
    for (file_name, path) in local_secret_files(dir)? {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("reading secret {file_name}: {e}"))?;
        let spec = parse_local_secret_spec(&file_name, &text)?;
        let value = resolve_local_secret_value(&file_name, &path, spec.value)?;
        // A declared secret whose environment variable is not set is not
        // present: the caller asks this to decide whether a credential is
        // available, and a declaration without bytes is not one.
        present |= spec.name == name && value.is_some();
    }
    Ok(present)
}

/// Read and resolve the caller's `.caos-secrets` store (design/secrets.md):
/// each reader resolved HERE (via eval-path, against the store's pinned tree)
/// to a partial arg tree of name → oid — so the server only subset-matches,
/// never evals. Empty when there is no store.
pub fn build_secret_store(t: &dyn Transport) -> Result<Vec<ClientSecret>, String> {
    let dir = Path::new(SECRETS_DIR);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let pinned = secrets_pinned_tree(t, dir)?;
    let mut store = Vec::new();
    for (file_name, path) in local_secret_files(dir)? {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("reading secret {file_name}: {e}"))?;
        let spec = parse_local_secret_spec(&file_name, &text)?;
        let Some(value) = resolve_local_secret_value(&file_name, &path, spec.value)? else {
            warn_unset_secret_env(&file_name, "value");
            continue;
        };
        // No entropy, no secret. An empty `entropy` would put every user of
        // this declaration on the same `secret-hash`, so a declaration whose
        // entropy variable is unset is dropped exactly as a valueless one is —
        // the isolation is not optional, and silently running without it is
        // the failure this form was added to avoid.
        let entropy = match spec.entropy {
            Some(LocalEntropy::Literal(literal)) => literal,
            Some(LocalEntropy::Env(var)) => match non_empty_env(&var) {
                Some(entropy) => entropy,
                None => {
                    warn_unset_secret_env(&file_name, "entropy");
                    continue;
                }
            },
            None => String::new(),
        };
        let mut readers = Vec::new();
        for reader in &spec.readers {
            match resolve_reader_client(t, &pinned, reader)? {
                Some(entries) => readers.push(entries),
                // A reader naming a path the tree does not carry grants nothing
                // — no arg tree can be a superset of an image that does not
                // exist — so it is DROPPED rather than failing the load. A store
                // is read by every client on every turn, so a reader that has
                // outlived the directory it named (a tool moved from
                // `caos-tools/` into `std/`) would otherwise take down `tui`,
                // `talk` and `run-tool` alike, over a grant that was already
                // inert. Dropping only narrows access, so it is fail-closed.
                None => warn_absent_reader(&file_name, reader),
            }
        }
        store.push(ClientSecret {
            name: spec.name,
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

/// Check the same reader and isolation conditions used by server injection.
/// Callers that require a credential can reject a prepared request before
/// admitting durable work. Merely having a secret in the store, or a hash from
/// some other secret in the request, does not authorize this credential.
pub fn client_request_has_secret(
    t: &dyn Transport,
    request: &str,
    store: &[ClientSecret],
    name: &str,
) -> Result<bool, String> {
    let entries = fetch_tree_entries(t, request)?
        .ok_or_else(|| format!("request {request} is not an ArgTree"))?
        .into_iter()
        .map(|entry| {
            (
                String::from_utf8_lossy(entry_name(&entry)).into_owned(),
                entry.oid.to_string(),
            )
        })
        .collect();
    request_has_secret(&entries, store, name)
}

fn request_has_secret(
    entries: &std::collections::BTreeMap<String, String>,
    store: &[ClientSecret],
    name: &str,
) -> Result<bool, String> {
    if !store.iter().any(|secret| {
        secret.name == name
            && secret
                .readers
                .iter()
                .any(|reader| reader_subset(reader, entries))
    }) {
        return Ok(false);
    }
    let Some(digest) = client_secret_hash(store, entries)? else {
        return Ok(false);
    };
    let expected = hash_bytes("blob", digest.as_bytes())?.to_string();
    Ok(entries.get(caos_world::SECRET_HASH_ARG) == Some(&expected))
}

/// A stable in-process identity for a secret store: everything about it that can
/// change an evaluation's answer, and nothing else.
///
/// Used to key the evaluation memos ([`eval::Memo`]), which must not let a run
/// resolved under one store answer for another. What a store contributes to a
/// result is exactly what [`client_secret_hash`] reads — each secret's `name`,
/// its `entropy`, and the readers deciding whether it matches — so those are the
/// key. **The `value` is deliberately absent**: it is not in the arg tree the
/// server caches on either (only the digest is), so including it would key on
/// something that cannot change the answer while putting secret material into a
/// long-lived map.
pub(crate) fn store_key(store: &[ClientSecret]) -> String {
    let mut key = String::new();
    for secret in store {
        key.push_str(&secret.name);
        key.push('\u{1}');
        key.push_str(&secret.entropy);
        for reader in &secret.readers {
            key.push('\u{2}');
            for (name, oid) in reader {
                key.push_str(name);
                key.push('=');
                key.push_str(oid);
                key.push('\u{3}');
            }
        }
        key.push('\u{4}');
    }
    key
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
    let image_entry = base_arg_entry(t, &image_ref)?;
    let mut base: std::collections::BTreeMap<String, String> = bound
        .iter()
        .map(|e| {
            (
                String::from_utf8_lossy(entry_name(e)).into_owned(),
                e.oid.to_string(),
            )
        })
        .collect();
    base.insert("base".to_string(), image_entry.oid.to_string());
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
        .ok_or_else(|| "this transport cannot ingest the source tree for secrets".to_string())?;
    Ok(oid.to_string())
}

/// Sorted, visible secret files shared by the offline `secrets` command and the
/// runtime loader. Dotfiles (`.tree`, editor backups) and non-files are metadata,
/// not secrets.
fn local_secret_files(dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("reading {SECRETS_DIR}: {e}"))? {
        let path = entry
            .map_err(|e| format!("reading {SECRETS_DIR}: {e}"))?
            .path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if !name.starts_with('.') && path.is_file() {
            files.push((name, path));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

enum LocalSecretValue {
    Inline(String),
    File(String),
    /// `value:env=<VAR>` — the bytes come from the process environment, so the
    /// file declaring them carries no secret and can be COMMITTED. That is the
    /// point of the form: a shared starter repo ships the declaration (the
    /// name, the readers) and each user supplies their own value.
    Env(String),
}

/// A secret's cache-isolation entropy: a literal, or `entropy:env=<VAR>`.
///
/// It gets the same env form as the value because it is a capability and not a
/// label — SPEC: "the entropy is a bearer capability for the cache: knowing it
/// reconstructs the key of any run that used it". A committed literal beside a
/// committed `value:env=` would hand every fork of that repo one `secret-hash`,
/// which is the isolation `secret-hash` exists to provide.
enum LocalEntropy {
    Literal(String),
    Env(String),
}

struct LocalSecretSpec {
    name: String,
    value: Option<LocalSecretValue>,
    entropy: Option<LocalEntropy>,
    readers: Vec<String>,
}

/// Parse the repeated-key secret-file format once for both `caos-cli secrets`
/// and runtime loading. Value files remain unresolved here so the offline
/// entropy command does not need to read secret material.
fn parse_local_secret_spec(file_name: &str, text: &str) -> Result<LocalSecretSpec, String> {
    let mut name = file_name.to_string();
    let mut value = None;
    let mut entropy = None;
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
            "entropy" => entropy = Some(LocalEntropy::Literal(val.trim().to_string())),
            "entropy:env" => entropy = Some(LocalEntropy::Env(val.trim().to_string())),
            "value" => value = Some(LocalSecretValue::Inline(val.to_string())),
            "value:@" => value = Some(LocalSecretValue::File(val.to_string())),
            "value:env" => value = Some(LocalSecretValue::Env(val.trim().to_string())),
            "reader" => readers.push(val.trim().to_string()),
            other => return Err(format!("secret {file_name}: unknown key {other:?}")),
        }
    }
    // THE ONE COMBINATION THAT IS REFUSED, and it is refused here so that every
    // caller refuses it: a value from the environment is the committed-file
    // form, and a literal `entropy=` in a committed file is the same bearer
    // capability in every clone of it. Two users with different tokens would
    // then compute the SAME `secret-hash` and share cache keys — exactly what
    // SPEC's "avoid accidentally sharing data derived from secrets through the
    // cache" forbids.
    //
    // An ERROR rather than the drop-with-warning an absent reader gets, because
    // the two fail in opposite directions: dropping a reader only narrows
    // access, while a shared entropy widens what one user's cache exposes to
    // another. Fail-closed means stopping here.
    if let (Some(LocalSecretValue::Env(var)), Some(LocalEntropy::Literal(_))) = (&value, &entropy) {
        return Err(format!(
            "secret {file_name}: value:env={var} with a literal entropy= — a file whose VALUE \
             comes from the environment is meant to be committed, and a committed entropy is a \
             cache capability every clone would share. Use entropy:env=<VAR> as well."
        ));
    }
    Ok(LocalSecretSpec {
        name,
        value,
        entropy,
        readers,
    })
}

/// The secret's bytes, or `None` when an `:env=` form names a variable this
/// process does not have.
///
/// `None` is not an error, and that is deliberate. The committed-file form
/// exists so a shared starter repo can DECLARE a secret that most users will
/// never set — a `github-token` that only private imports need. Erroring on an
/// unset variable would make that declaration break every turn for everyone
/// who does not need it. Dropping it instead leaves the worker to fail on a
/// missing `/secret/<name>` if it really wanted one, which is the contract SPEC
/// already states ("A worker must fail if the secret is missing or invalid").
fn resolve_local_secret_value(
    file_name: &str,
    path: &Path,
    value: Option<LocalSecretValue>,
) -> Result<Option<String>, String> {
    match value.ok_or_else(|| format!("secret {file_name}: no value= line"))? {
        LocalSecretValue::Inline(value) => Ok(Some(value)),
        LocalSecretValue::File(value_path) => {
            let file = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(&value_path);
            let bytes = std::fs::read(&file)
                .map_err(|e| format!("secret {file_name} value:@={value_path}: {e}"))?;
            String::from_utf8(bytes)
                .map(Some)
                .map_err(|e| format!("secret {file_name} value not UTF-8: {e}"))
        }
        // TRIMMED, unlike the other two forms, which are verbatim by contract.
        // An environment variable is set by a shell, and `export T=$(cat tok)`
        // or a copied-in value carrying a newline is far likelier than a token
        // that genuinely ends in whitespace. Untrimmed, that newline rides into
        // the Authorization header and comes back as a 401 that reads as a bad
        // token rather than a bad variable.
        LocalSecretValue::Env(var) => Ok(non_empty_env(&var)),
    }
}

/// A trimmed environment variable, or `None` when it is unset or blank. Blank
/// counts as unset: an exported-but-empty variable is a value nothing can use,
/// and treating it as present would ship an empty credential.
fn non_empty_env(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => None,
    }
}

/// Resolve a reader — a single path/expression, no argument pins
/// (design/secrets.md: a reader names an *expression*; narrow by pointing at a
/// narrower one, not by pinning args here) — to the partial arg tree it stands
/// for: eval-path the path (so a flake/`.caos-expr` tool resolves to the same
/// arg tree the run uses), unwrap any curry layers, and take its entries. That
/// tree already carries whatever the expression bakes in (e.g. a curried
/// `worker1` script), so it is as specific as the expression is.
///
/// `None` when the tree carries no such path: an OPTIONAL reader, see
/// [`build_secret_store`].
fn resolve_reader_client(
    t: &dyn Transport,
    pinned: &str,
    reader: &str,
) -> Result<Option<std::collections::BTreeMap<String, String>>, String> {
    if reader.split_whitespace().count() != 1 {
        return Err(format!(
            "reader {reader:?} must be a single path (argument pins are not supported — \
             point at a narrower expression instead)"
        ));
    }
    let Some(image) = resolve_reader_image(t, pinned, reader.trim())? else {
        return Ok(None);
    };
    let (base, bound) = unwrap_curry(t, &image)?;
    let mut entries = std::collections::BTreeMap::new();
    for entry in bound {
        entries.insert(
            String::from_utf8_lossy(entry_name(&entry)).into_owned(),
            entry.oid.to_string(),
        );
    }
    // The image entry wins over any like-named bound arg, mirroring assembly.
    entries.insert("base".to_string(), base);
    Ok(Some(entries))
}

/// Whether an `eval-path` failure means the TREE simply does not carry the
/// reader's path, as opposed to the expression at that path being broken.
///
/// The walk reports a missing component as `eval-path: "<name>" not found in
/// <tree oid>`; the other "not found" messages name a path *inside* an
/// expression and end in `in tree`. Only the first is an absent reader — a
/// reader that resolves to a `.caos-expr` which then fails must stay loud.
fn reader_path_absent(error: &str) -> bool {
    error
        .strip_prefix("eval-path: ")
        .and_then(|rest| rest.rsplit_once(" not found in "))
        .is_some_and(|(_, node)| is_hex_hash(node))
}

/// Report an absent reader ONCE per process. Once, because the store is rebuilt
/// on every turn while an interactive client owns the terminal: the first load
/// happens before the TUI takes the screen, so the notice lands at the shell
/// prompt instead of being repainted over inside a frame.
/// Once per process, like [`warn_absent_reader`]: a store is rebuilt on every
/// turn and every tool call, so an unset variable would otherwise print on each
/// one. Said at all because the alternative — a declared secret that quietly
/// is not in the store — is the kind of absence a worker reports much later as
/// a permission error against GitHub.
fn warn_unset_secret_env(secret: &str, field: &str) {
    static WARNED: OnceLock<Mutex<std::collections::BTreeSet<String>>> = OnceLock::new();
    let mut warned = WARNED
        .get_or_init(|| Mutex::new(std::collections::BTreeSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if warned.insert(format!("{secret}\u{0}{field}")) {
        eprintln!(
            "caos: secret {secret}: its {field}:env= variable is unset or empty — the secret is \
             not in this store (set it to use the secret; ignore this if you do not need it)"
        );
    }
}

fn warn_absent_reader(secret: &str, reader: &str) {
    static WARNED: OnceLock<Mutex<std::collections::BTreeSet<String>>> = OnceLock::new();
    let mut warned = WARNED
        .get_or_init(|| Mutex::new(std::collections::BTreeSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if warned.insert(format!("{secret}\u{0}{reader}")) {
        eprintln!(
            "caos: secret {secret}: reader {reader:?} names no path in this tree — ignored \
             (it grants nothing; drop the line or point it at the path that replaced it)"
        );
    }
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
///
/// `None` when that path is not in the tree ([`reader_path_absent`]).
fn resolve_reader_image(
    t: &dyn Transport,
    pinned: &str,
    expr: &str,
) -> Result<Option<String>, String> {
    if is_hex_hash(expr) {
        return Ok(Some(expr.to_string()));
    }
    // Empty store: a reader's own resolution must not be marked (its arg tree is
    // what the match compares against; marking it would be circular).
    match eval::eval_path(t, pinned, expr, &[]) {
        Ok((_, oid)) => Ok(Some(oid)),
        Err(error) if reader_path_absent(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn request_compute(base: &str, arg_tree: &str, secrets: &str) -> Result<(String, String), String> {
    request_compute_url(base, &run_path(arg_tree), secrets)
}

/// Run an already-prepared request without a secret-store header.
pub fn compute_client_request(base: &str, arg_tree: &str) -> Result<(String, String), String> {
    request_compute(base, arg_tree, "")
}

/// Run an already-prepared request with the supplied local secret store carried
/// in ephemeral request context.
pub fn compute_client_request_with_store(
    base: &str,
    arg_tree: &str,
    store: &[ClientSecret],
) -> Result<(String, String), String> {
    request_compute(base, arg_tree, &secret_store_header(store))
}

/// `caos resolve-image <hex hash | docker://ref>` — print the reference a
/// runner would pull for this image.
///
/// The point is the git-docker case: the server converts the tree (base +
/// `layer<NN>` + config) into a registry digest and caches it, but until now that
/// digest was reachable only by RUNNING the image. Anything that wants to hand
/// the converted image somewhere else — copy it to another registry, submit it
/// to a platform — had to rebuild it, duplicating `convert_git_image`'s
/// layer/diff_id/manifest arithmetic. This exposes what the server already
/// computed.
pub fn caos_resolve_image(args: &[String]) -> Result<(), String> {
    let image = args
        .first()
        .ok_or("usage: resolve-image <hex hash | docker://<ref>>")?;
    // `&` and `#` would split the query; nothing else in a hex hash or a
    // docker reference needs escaping, and the server percent-decodes anyway.
    if image.contains('&') || image.contains('#') {
        return Err(format!("image reference cannot contain & or #: {image:?}"));
    }
    let body = server_get(&server_url()?, &format!("/resolve-image?image={image}"))?;
    let reference =
        String::from_utf8(body).map_err(|e| format!("server returned invalid UTF-8: {e}"))?;
    println!("{}", reference.trim());
    Ok(())
}

/// The one shape every compute path uses. `req` is the query param's historical
/// name; its value is the ArgTree hash.
fn run_path(arg_tree: &str) -> String {
    format!("/run?req={arg_tree}")
}

/// Ask the server to start `arg_tree` with the current in-flight job's
/// un-hashed context. The response acknowledges admission only; the sub-run
/// continues on a server thread after this call returns.
fn request_sub_run(base: &str, arg_tree: &str, nonce: &str) -> Result<(), String> {
    let body = serde_json::json!({"req": arg_tree, "nonce": nonce}).to_string();
    server_call(
        base,
        &ServerRequest {
            method: "POST",
            path: "/sub-run",
            headers: &[("content-type", "application/json".to_string())],
            body: Some(body.as_bytes()),
            timeout_secs: Some(5),
        },
    )?;
    Ok(())
}

/// Issue the compute `GET /run`, carrying the ephemeral secrets store in the
/// [`SECRETS_HEADER`] header when non-empty (design/secrets.md) — out of band
/// from the content-addressed ArgTree in the URL.
fn request_compute_url(base: &str, path: &str, secrets: &str) -> Result<(String, String), String> {
    let headers = if secrets.is_empty() {
        Vec::new()
    } else {
        vec![(SECRETS_HEADER, secrets.to_string())]
    };
    // NO TIMEOUT, deliberately: this is the call that waits for the work. A run
    // takes as long as the worker does.
    let body = server_call(
        base,
        &ServerRequest {
            method: "GET",
            path,
            headers: &headers,
            body: None,
            timeout_secs: None,
        },
    )?;
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

#[cfg(test)]
mod git_transport_tests {
    use super::*;
    use std::process::Command;

    struct ObjectTransport {
        object: Option<(&'static str, Vec<u8>)>,
    }

    impl Transport for ObjectTransport {
        fn put_object(&self, _kind: &str, _content: &[u8]) -> Result<gix::ObjectId, String> {
            Err("unexpected put".to_string())
        }

        fn get_object(&self, hash: &str) -> Result<(String, Vec<u8>), String> {
            self.object
                .as_ref()
                .map(|(kind, content)| ((*kind).to_string(), content.clone()))
                .ok_or_else(|| format!("missing {hash}"))
        }

        fn has_object(&self, _hash: &str) -> Result<bool, String> {
            Ok(self.object.is_some())
        }

        fn server_url(&self) -> Result<String, String> {
            Err("unexpected server URL lookup".to_string())
        }
    }

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

    #[test]
    fn projected_commits_preserve_identity_and_ancestry_through_file_operations() {
        // Isolate the CAS env from other parallel unit tests.
        const CHILD: &str = "CAOS_PROJECTION_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = TestDir::new("projection");
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "git_transport_tests::projected_commits_preserve_identity_and_ancestry_through_file_operations", "--nocapture"])
                .env(CHILD, "1").env(CAS_DIR_ENV, dir.path().join("cas"))
                .status().unwrap();
            assert!(status.success());
            return;
        }
        let dir = TestDir::new("projection-repo");
        init_repo(dir.path());
        std::fs::write(
            dir.path().join("code"),
            "original
",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("run"),
            "#!/bin/sh
",
        )
        .unwrap();
        set_mode(&dir.path().join("run"), 0o755).unwrap();
        std::os::unix::fs::symlink("code", dir.path().join("link")).unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "base"]);
        let base = git(dir.path(), &["rev-parse", "HEAD"]);
        let t = GitTransport::discover(dir.path()).unwrap();
        let (_, raw) = t.get_object(base.trim()).unwrap();
        let signed = String::from_utf8(raw).unwrap().replacen(
            "

",
            "
gpgsig -----BEGIN PGP SIGNATURE-----
 fixture
 -----END PGP SIGNATURE-----

",
            1,
        );
        let base = t
            .put_object("commit", signed.as_bytes())
            .unwrap()
            .to_string();
        let cas = cas_dir();
        std::fs::create_dir_all(&cas).unwrap();
        let original = cas.join("original");
        get_hash(&t, &base, original.to_str().unwrap()).unwrap();
        let root = dir.path().join("conversation");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&original, root.join("dirty")).unwrap();
        std::fs::write(root.join("memory"), "remember").unwrap();
        let (_, tree) = store(&t, Some(&cas), &root).unwrap();
        let (kind, resolved) = eval::eval_path(&t, &tree.to_string(), "dirty/code", &[]).unwrap();
        assert_eq!(kind, "blob");
        assert_eq!(t.get_object(&resolved).unwrap().0, "blob");
        assert_eq!(
            eval::eval_path(&t, &tree.to_string(), "dirty", &[]).unwrap(),
            ("commit".into(), base.clone())
        );
        let projection = dir.path().join("projection");
        // Model the harness's writable directory metadata; put itself does
        // not materialize editable files.
        let source = cas.join(format!("checkout-{base}"));
        get_hash(&t, &base, source.to_str().unwrap()).unwrap();
        std::fs::create_dir(&projection).unwrap();
        let project_fixture = |name: &str| {
            let directory = projection.join(name);
            std::fs::create_dir(&directory).unwrap();
            for name in ["code", "run"] {
                std::fs::copy(dir.path().join(name), directory.join(name)).unwrap();
            }
            std::os::unix::fs::symlink("code", directory.join("link")).unwrap();
            xattr::set(&directory, "user.caos.commit", base.as_bytes()).unwrap();
        };
        project_fixture("dirty");
        std::fs::write(projection.join("memory"), "remember").unwrap();
        assert_eq!(store(&t, Some(&cas), &projection).unwrap().1, tree);
        project_fixture("copy");
        std::fs::rename(projection.join("copy"), projection.join("review")).unwrap();
        std::fs::write(
            projection.join("dirty/code"),
            "edited
",
        )
        .unwrap();
        std::fs::write(projection.join("memory"), "updated").unwrap();
        let (_, changed) = store(&t, Some(&cas), &projection).unwrap();
        let (_, bytes) = t.get_object(&changed.to_string()).unwrap();
        let entries = gix::objs::TreeRef::from_bytes(&bytes, gix::hash::Kind::Sha1).unwrap();
        let entry = |name: &[u8]| entries.entries.iter().find(|e| e.filename == name).unwrap();
        assert_eq!(
            entry(b"review").oid.to_string(),
            base,
            "copied and renamed boundaries preserve the exact signed commit"
        );
        assert_eq!(
            entry(b"dirty").mode.kind(),
            gix::objs::tree::EntryKind::Commit
        );
        let (_, edited) = t.get_object(&entry(b"dirty").oid.to_string()).unwrap();
        let edited = String::from_utf8(edited).unwrap();
        assert!(edited.contains(&format!(
            "parent {base}
"
        )));
        assert!(
            !edited.contains("gpgsig"),
            "new content must not retain the old signature"
        );
        let resolved = cas.join("resolved");
        gitlinks::resolve(
            &t,
            &changed.to_string(),
            "dirty/run",
            resolved.to_str().unwrap(),
        )
        .unwrap();
        assert_ne!(
            std::fs::metadata(&resolved).unwrap().permissions().mode() & 0o111,
            0
        );
        assert!(gitlinks::resolve(
            &t,
            &changed.to_string(),
            "../code",
            resolved.to_str().unwrap()
        )
        .is_err());
        assert!(projection.join("dirty/link").is_symlink());
        // Clearing or deleting a resolved ledger cleans only source-tree metadata.
        let metadata = projection.join("dirty/.caos");
        std::fs::create_dir(&metadata).unwrap();
        std::fs::write(metadata.join("conflicts"), "unresolved code\n").unwrap();
        let (_, unresolved) = store(&t, Some(&cas), &projection).unwrap();
        let (_, unresolved) = eval::eval_path(&t, &unresolved.to_string(), "dirty", &[]).unwrap();
        assert_eq!(
            git(
                dir.path(),
                &["show", &format!("{unresolved}:.caos/conflicts")]
            ),
            "unresolved code\n"
        );
        let (_, raw) = t.get_object(&unresolved).unwrap();
        let merge = String::from_utf8(raw).unwrap().replacen(
            &format!("parent {base}\n"),
            &format!("parent {base}\nparent {}\n", entry(b"dirty").oid),
            1,
        );
        let merge = t
            .put_object("commit", merge.as_bytes())
            .unwrap()
            .to_string();
        get_hash(
            &t,
            &merge,
            cas.join(format!("checkout-{merge}")).to_str().unwrap(),
        )
        .unwrap();
        xattr::set(
            projection.join("dirty"),
            "user.caos.commit",
            merge.as_bytes(),
        )
        .unwrap();
        let (_, unchanged) = store(&t, Some(&cas), &projection).unwrap();
        assert_eq!(
            eval::eval_path(&t, &unchanged.to_string(), "dirty", &[])
                .unwrap()
                .1,
            merge
        );

        // Ordinary conversation files are outside this cleanup rule.
        std::fs::create_dir(projection.join(".caos")).unwrap();
        std::fs::write(projection.join(".caos/conflicts"), "").unwrap();
        for remove_ledger in [false, true] {
            if remove_ledger {
                std::fs::remove_file(metadata.join("conflicts")).unwrap();
            } else {
                std::fs::write(metadata.join("conflicts"), "").unwrap();
            }
            let (_, cleaned) = store(&t, Some(&cas), &projection).unwrap();
            let (_, source) = eval::eval_path(&t, &cleaned.to_string(), "dirty", &[]).unwrap();
            assert!(git(dir.path(), &["ls-tree", &source, "--", ".caos"]).is_empty());
            assert_eq!(
                git(dir.path(), &["rev-parse", &format!("{source}^")]).trim(),
                merge
            );
            assert!(eval::eval_path(&t, &cleaned.to_string(), ".caos/conflicts", &[]).is_ok());
        }
        std::fs::write(metadata.join("conflicts"), "").unwrap();
        std::fs::write(metadata.join("other"), "keep").unwrap();
        let (_, retained) = store(&t, Some(&cas), &projection).unwrap();
        assert!(eval::eval_path(&t, &retained.to_string(), "dirty/.caos/conflicts", &[]).is_err());
        assert!(eval::eval_path(&t, &retained.to_string(), "dirty/.caos/other", &[]).is_ok());
    }

    #[test]
    fn all_compute_paths_share_one_request_shape() {
        assert_eq!(
            run_path(&"a".repeat(40)),
            format!("/run?req={}", "a".repeat(40))
        );
    }

    /// Records what the funnel hands a ticket transport, so the dispatch can be
    /// tested without a server. Installed once for this process — the other
    /// tests use `http://` bases, which never reach it.
    struct Recorder;

    /// base, method, path, headers.
    #[allow(clippy::type_complexity)]
    static RECORDED: std::sync::Mutex<Vec<(String, String, String, Vec<(String, String)>)>> =
        std::sync::Mutex::new(Vec::new());

    impl TicketTransport for Recorder {
        fn request(&self, base: &str, request: &ServerRequest) -> Result<ServerResponse, String> {
            RECORDED.lock().unwrap().push((
                base.to_string(),
                request.method.to_string(),
                request.path.to_string(),
                request
                    .headers
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect(),
            ));
            Ok(ServerResponse {
                status: 200,
                reason: "OK".to_string(),
                body: b"recorded".to_vec(),
            })
        }
    }

    #[test]
    fn a_ticket_base_reaches_the_installed_transport_with_no_doubled_slash() {
        install_ticket_transport(Box::new(Recorder));
        // A trailing slash on the base must not double up against the path,
        // whichever transport carries it.
        let body = server_get(&format!("{TICKET_SCHEME}endpointaaa.0011/"), "/object/abc")
            .expect("the recorder answers");
        assert_eq!(body, b"recorded");
        let recorded = RECORDED.lock().unwrap();
        let (base, method, path, headers) = recorded.last().expect("one call");
        assert_eq!(base, &format!("{TICKET_SCHEME}endpointaaa.0011"));
        assert_eq!(method, "GET");
        assert_eq!(path, "/object/abc");
        // The world tag rides on this transport too. A missing one is ACCEPTED by
        // the server, so nothing else would notice its absence — and what it
        // would let through is a host client driving a test stack.
        assert!(
            headers.iter().any(|(name, value)| name
                == caos_world::WORLD_HEADER
                && value == caos_world::WORLD),
            "no world header: {headers:?}"
        );
    }

    #[test]
    fn a_server_that_is_neither_http_nor_a_ticket_says_so() {
        let error = server_get("git@example.com:repo.git", "/object/abc").expect_err("refused");
        assert!(
            error.contains("neither an HTTP URL nor a ticket"),
            "{error}"
        );
    }

    #[test]
    fn sub_run_rejects_noncanonical_and_nonrunnable_requests_before_dispatch() {
        let request = "a".repeat(40);
        let missing = ObjectTransport { object: None };
        assert!(caos_sub_run(&missing, &request)
            .unwrap_err()
            .contains("already-stored ArgTree"));

        let blob = ObjectTransport {
            object: Some(("blob", b"not a request".to_vec())),
        };
        assert!(caos_sub_run(&blob, &request)
            .unwrap_err()
            .contains("is a blob"));

        let curry_or_plain_tree = ObjectTransport {
            object: Some(("tree", Vec::new())),
        };
        assert!(caos_sub_run(&curry_or_plain_tree, &request)
            .unwrap_err()
            .contains("has no 'base' entry"));

        let uppercase = request.to_ascii_uppercase();
        assert!(caos_sub_run(&missing, &uppercase)
            .unwrap_err()
            .contains("lowercase 40-character"));
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

        // Fetches may create packs after the transport was opened. Exceed
        // gix's initial 32 slots, then exercise writing with the same handle.
        let git_input = |args: &[&str], input: &[u8]| {
            let mut child = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            std::io::Write::write_all(&mut child.stdin.take().unwrap(), input).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        };
        for index in 0..40 {
            let hash = git_input(
                &["hash-object", "-w", "--stdin"],
                format!("pack {index}").as_bytes(),
            );
            git_input(&["pack-objects", ".git/objects/pack/pack"], hash.as_bytes());
        }
        let blob = transport.put_object("blob", b"after fetch").unwrap();
        assert_eq!(
            transport.get_object(&blob.to_string()).unwrap().1,
            b"after fetch"
        );

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

#[cfg(test)]
mod local_secret_tests {
    use super::{parse_local_secret_spec, LocalEntropy, LocalSecretValue};

    fn literal_entropy(spec: &super::LocalSecretSpec) -> Option<&str> {
        match &spec.entropy {
            Some(LocalEntropy::Literal(literal)) => Some(literal.as_str()),
            _ => None,
        }
    }

    #[test]
    fn one_parser_serves_entropy_maintenance_and_runtime_loading() {
        let spec = parse_local_secret_spec(
            "token",
            "name=api-key\nvalue:@=../key\nentropy=abc123\nreader=DEEP-DEPS/tool\n",
        )
        .unwrap();
        assert_eq!(spec.name, "api-key");
        assert_eq!(literal_entropy(&spec), Some("abc123"));
        assert_eq!(spec.readers, ["DEEP-DEPS/tool"]);
        match spec.value {
            Some(LocalSecretValue::File(path)) => assert_eq!(path, "../key"),
            _ => panic!("value:@ was not preserved as an unresolved file value"),
        }
    }

    /// The committed-file form: both halves come from the environment, so the
    /// file itself carries neither the bytes nor the cache capability.
    #[test]
    fn env_forms_name_variables_rather_than_carrying_values() {
        let spec = parse_local_secret_spec(
            "github-token",
            "value:env=GITHUB_TOKEN\nentropy:env=CAOS_GITHUB_TOKEN_ENTROPY\nreader=caos-std/llm-step\n",
        )
        .unwrap();
        assert_eq!(spec.name, "github-token");
        assert_eq!(spec.readers, ["caos-std/llm-step"]);
        match spec.value {
            Some(LocalSecretValue::Env(var)) => assert_eq!(var, "GITHUB_TOKEN"),
            _ => panic!("value:env was not parsed as an environment value"),
        }
        match spec.entropy {
            Some(LocalEntropy::Env(var)) => assert_eq!(var, "CAOS_GITHUB_TOKEN_ENTROPY"),
            _ => panic!("entropy:env was not parsed as an environment entropy"),
        }
    }

    /// The whole reason `entropy:env=` exists: a committed literal would be one
    /// bearer capability shared by every clone, so the pairing is refused at
    /// parse time rather than warned about at use time.
    #[test]
    fn a_committed_value_may_not_carry_a_literal_entropy() {
        let error = parse_local_secret_spec(
            "github-token",
            "value:env=GITHUB_TOKEN\nentropy=0123456789abcdef\n",
        )
        .err()
        .expect("a committed literal entropy must be refused");
        assert!(error.contains("value:env=GITHUB_TOKEN"), "{error}");
        assert!(error.contains("entropy:env="), "{error}");
    }

    /// The reverse pairing is fine: a gitignored value file beside an entropy
    /// the environment supplies narrows nothing and shares nothing.
    #[test]
    fn a_file_value_may_take_its_entropy_from_the_environment() {
        let spec = parse_local_secret_spec("token", "value:@=../key\nentropy:env=E\n").unwrap();
        assert!(matches!(spec.value, Some(LocalSecretValue::File(_))));
        assert!(matches!(spec.entropy, Some(LocalEntropy::Env(_))));
    }

    #[test]
    fn presence_checks_local_specs_and_values_without_evaluating_readers() {
        let dir = std::env::temp_dir().join(format!(
            "secret-presence-{}",
            super::fresh_entropy().unwrap()
        ));
        assert!(!super::local_secret_present(&dir, "api-key").unwrap());
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("token");
        std::fs::write(
            &file,
            "name=api-key\nvalue=test-value\nreader=missing-worker\n",
        )
        .unwrap();
        assert!(super::local_secret_present(&dir, "api-key").unwrap());
        assert!(!super::local_secret_present(&dir, "other").unwrap());
        std::fs::write(&file, "name=api-key\nvalue:@=missing.value\n").unwrap();
        assert!(super::local_secret_present(&dir, "api-key").is_err());
        std::fs::write(&file, "not a spec\n").unwrap();
        assert!(super::local_secret_present(&dir, "api-key").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn entropy_maintenance_can_parse_a_spec_before_its_value_is_complete() {
        let spec = parse_local_secret_spec("token", "reader=DEEP-DEPS/tool\n").unwrap();
        assert!(spec.value.is_none());
        assert!(spec.entropy.is_none());
    }

    #[test]
    fn shared_parser_rejects_unknown_fields() {
        let error = parse_local_secret_spec("token", "wat=nope\n")
            .err()
            .expect("unknown field was accepted");
        assert!(error.contains("unknown key \"wat\""), "{error}");
    }

    /// An optional reader is exactly "the tree has no such path", which is the
    /// walk's message — naming the node it looked in. Everything else, up to
    /// and including a `not found` raised by the reader's OWN expression, is a
    /// broken reader and has to keep failing the load.
    #[test]
    fn only_a_missing_tree_path_makes_a_reader_optional() {
        let oid = "2114331da99790fef932866de7176d408c5a6e19";
        assert!(super::reader_path_absent(&format!(
            "eval-path: \"caos-tools\" not found in {oid}"
        )));
        for loud in [
            "eval-path: path \"src\" not found in tree",
            "eval-path: base path \"std/bash\" not found in tree",
            "eval-path: cannot descend into \"tool\": the prefix evaluated to a blob",
            "eval-path: undefined variable $BASE",
            "transport: connection refused",
        ] {
            assert!(!super::reader_path_absent(loud), "{loud}");
        }
    }
}

#[cfg(test)]
mod memo_tests {
    use super::{
        client_secret_hash, eval::Memo, hash_bytes, request_has_secret, store_key, ClientSecret,
    };

    fn secret(name: &str, value: &str, entropy: &str, reader: &[(&str, &str)]) -> ClientSecret {
        ClientSecret {
            name: name.to_string(),
            value: value.to_string(),
            entropy: entropy.to_string(),
            readers: vec![reader
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect()],
        }
    }

    #[test]
    fn required_secret_checks_reader_and_isolation_before_dispatch() {
        let image = "a".repeat(40);
        let other_image = "b".repeat(40);
        let mut entries = std::collections::BTreeMap::from([
            ("base".to_string(), image.clone()),
            ("extra".to_string(), "c".repeat(40)),
        ]);
        let store = vec![secret(
            "model-key",
            "private",
            "entropy",
            &[("base", &image)],
        )];
        // Matching the image alone must not authorize an old, unmarked request.
        assert!(!request_has_secret(&entries, &store, "model-key").unwrap());
        let digest = client_secret_hash(&store, &entries).unwrap().unwrap();
        entries.insert(
            caos_world::SECRET_HASH_ARG.to_string(),
            hash_bytes("blob", digest.as_bytes()).unwrap().to_string(),
        );
        assert!(request_has_secret(&entries, &store, "model-key").unwrap());
        assert!(!request_has_secret(&entries, &store, "other-key").unwrap());

        let wrong_reader = vec![secret(
            "model-key",
            "private",
            "entropy",
            &[("base", &other_image)],
        )];
        assert!(!request_has_secret(&entries, &wrong_reader, "model-key").unwrap());
        let mut absent_reader = secret("model-key", "private", "entropy", &[]);
        absent_reader.readers.clear();
        assert!(!request_has_secret(&entries, &[absent_reader], "model-key").unwrap());

        let rotated = vec![secret(
            "model-key",
            "replacement",
            "entropy",
            &[("base", &image)],
        )];
        assert!(request_has_secret(&entries, &rotated, "model-key").unwrap());
        let changed_identity = vec![secret(
            "model-key",
            "private",
            "new entropy",
            &[("base", &image)],
        )];
        assert!(!request_has_secret(&entries, &changed_identity, "model-key").unwrap());
    }

    /// The claim `store_key`'s doc comment makes: a store keys an evaluation by
    /// what decides the answer, and a secret's VALUE is not that. Two callers
    /// holding the same grant with different values form the same arg tree, so
    /// they must share a memo entry rather than each paying the round trips.
    #[test]
    fn a_secrets_value_does_not_key_an_evaluation() {
        let one = [secret("token", "hunter2", "e", &[("base", "abc")])];
        let two = [secret("token", "correct-horse", "e", &[("base", "abc")])];
        assert_eq!(store_key(&one), store_key(&two));
    }

    /// …and everything that DOES decide it separates the keys, so a run
    /// resolved under one grant can never answer for another.
    #[test]
    fn name_entropy_and_readers_each_key_an_evaluation() {
        let base = [secret("token", "v", "e", &[("base", "abc")])];
        let key = store_key(&base);
        for other in [
            [secret("other", "v", "e", &[("base", "abc")])],
            [secret("token", "v", "rotated", &[("base", "abc")])],
            [secret("token", "v", "e", &[("base", "def")])],
        ] {
            assert_ne!(key, store_key(&other));
        }
        assert_ne!(key, store_key(&[]));
    }

    /// Distinct keys are distinct answers, and a stored one comes back — the
    /// whole contract `eval_path` leans on.
    #[test]
    fn memo_answers_only_the_key_it_stored() {
        static M: Memo<String> = Memo::new();
        assert_eq!(M.get("a"), None);
        M.put("a".to_string(), "one".to_string());
        assert_eq!(M.get("a"), Some("one".to_string()));
        assert_eq!(M.get("b"), None);
    }
}

#[cfg(test)]
mod tool_resolution_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    #[derive(Default)]
    struct Store {
        objects: RefCell<HashMap<String, (String, Vec<u8>)>>,
        remote: RefCell<Option<gix::ObjectId>>,
        fetches: Cell<usize>,
        computes: Cell<usize>,
    }
    impl Transport for Store {
        fn put_object(&self, kind: &str, bytes: &[u8]) -> Result<gix::ObjectId, String> {
            let oid = hash_bytes(kind, bytes)?;
            self.objects
                .borrow_mut()
                .insert(oid.to_string(), (kind.into(), bytes.to_vec()));
            Ok(oid)
        }
        fn get_object(&self, oid: &str) -> Result<(String, Vec<u8>), String> {
            self.objects
                .borrow()
                .get(oid)
                .cloned()
                .ok_or_else(|| format!("missing {oid}"))
        }
        fn has_object(&self, oid: &str) -> Result<bool, String> {
            Ok(self.objects.borrow().contains_key(oid))
        }
        fn server_url(&self) -> Result<String, String> {
            self.computes.set(self.computes.get() + 1);
            Err("fixture forbids dispatch: target expression was evaluated".into())
        }
        fn fetch_git_ref(&self, _: &str, _: &str) -> Result<Option<gix::ObjectId>, String> {
            self.fetches.set(self.fetches.get() + 1);
            Ok(*self.remote.borrow())
        }
    }
    fn tree(t: &Store, entries: &[(&str, &str, &str)]) -> String {
        post_tree(
            t,
            entries
                .iter()
                .map(|(name, kind, oid)| gix::objs::tree::Entry {
                    mode: eval::mode_of_kind(kind),
                    filename: name.as_bytes().into(),
                    oid: parse_oid(oid).unwrap(),
                })
                .collect(),
        )
        .unwrap()
        .to_string()
    }
    fn expr(t: &Store, text: &str) -> String {
        let base = tree(t, &[]);
        let text = text
            .replace("--base=fixture", &format!("--base:hash={base}"))
            .replace("--base=forbidden", &format!("--base:hash={base}"));
        let blob = t.put_object("blob", text.as_bytes()).unwrap().to_string();
        tree(t, &[(".caos-expr", "blob", &blob)])
    }
    fn generated(t: &Store, tool: &str) -> String {
        // The original root has no args/ directory at all. Evaluation creates it.
        expr(t, &format!("curry --base=fixture --tool:hash={tool}"))
    }

    fn evaluated_help(t: &Store, root: &str, path: &str) -> (String, String) {
        let image = eval_tree_tool(t, root, path, &[]).unwrap();
        let mut node = image.clone();
        for name in ["args", "help"] {
            node = fetch_tree_entries(t, &node)
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|e| entry_name(e) == name.as_bytes())
                .unwrap()
                .oid
                .to_string();
        }
        let (_, bytes) = t.get_object(&node).unwrap();
        (image, String::from_utf8(bytes).unwrap())
    }

    #[test]
    fn help_evaluates_the_generated_target() {
        let t = Store::default();
        let target = expr(&t, "HELP=<<END\nGenerated help.\n@param word The word.\nEND\ncurry --base=fixture --help=$HELP");
        let root = generated(&t, &target);
        assert!(fetch_tree_entries(&t, &root)
            .unwrap()
            .unwrap()
            .iter()
            .all(|e| entry_name(e) != b"args"));
        let (image, help) = evaluated_help(&t, &root, "args/tool");
        assert_ne!(image, target);
        assert!(help.contains("Generated help."));
        assert!(help.contains("@param word The word."));
    }

    #[test]
    fn target_evaluation_failures_are_returned() {
        let t = Store::default();
        let target = expr(&t, "run --base=forbidden --help=Failing");
        let root = generated(&t, &target);
        assert!(eval_tree_tool(&t, &root, "args/tool", &[])
            .unwrap_err()
            .contains("target expression was evaluated"));
        assert_eq!(t.computes.get(), 1);
    }

    #[test]
    fn run_reaches_the_same_image_and_prepares_the_supplied_arguments() {
        let t = Store::default();
        let target = expr(&t, "HELP=<<END\nGenerated runner.\n@param word The word.\nEND\ncurry --base=fixture --help=$HELP");
        let root = generated(&t, &target);
        let evaluated = eval_tree_tool(&t, &root, "args/tool", &[]).unwrap();
        assert_eq!(evaluated_help(&t, &root, "args/tool").0, evaluated);
        assert_ne!(evaluated, target);
        let word = t.put_object("blob", b"supplied").unwrap();
        let request = assemble_arg_tree(
            &t,
            &evaluated,
            vec![
                gix::objs::tree::Entry {
                    mode: eval::mode_of_kind("blob"),
                    filename: b"word".to_vec().into(),
                    oid: word,
                },
                gix::objs::tree::Entry {
                    mode: eval::mode_of_kind("tree"),
                    filename: b"in".to_vec().into(),
                    oid: parse_oid(&root).unwrap(),
                },
            ],
            &[],
        )
        .unwrap();
        let entries = fetch_tree_entries(&t, &request).unwrap().unwrap();
        assert!(entries
            .iter()
            .any(|e| entry_name(e) == b"word" && e.oid == word));
        assert!(entries
            .iter()
            .any(|e| entry_name(e) == b"in" && e.oid.to_string() == root));
    }

    #[test]
    fn ordinary_and_missing_paths() {
        let t = Store::default();
        let target = expr(&t, "curry --base=fixture --help=Ordinary");
        let root = tree(&t, &[("tool", "tree", &target)]);
        assert_eq!(evaluated_help(&t, &root, "tool").1, "Ordinary");
        let error = eval_tree_tool(&t, &root, "missing", &[]).unwrap_err();
        assert!(error.contains("no such path:"));
        assert!(error.contains("missing"));
        assert!(error.contains("Directories in .: tool"));
        assert!(reader_path_absent(&error));
    }

    #[test]
    fn pinned_ancestor_dependencies_are_resolved_by_the_client() {
        let t = Store::default();
        let target = expr(&t, "curry --base=fixture --help=Pinned");
        let repo = tree(&t, &[("tool", "tree", &target)]);
        *t.remote.borrow_mut() = Some(parse_oid(&repo).unwrap());
        let root = expr(&t, "curry --base=fixture --repo:@@=git+https://example.invalid/tool-fixture?rev=1234567890123456789012345678901234567890");
        assert_eq!(evaluated_help(&t, &root, "args/repo/tool").1, "Pinned");
        assert_eq!(t.fetches.get(), 1);
    }
}
