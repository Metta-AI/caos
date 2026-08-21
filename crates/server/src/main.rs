//! caos server: storage and compute behind one endpoint.
//!
//! Storage (a tiny HTTP front-end over a git object database mounted at `/git`):
//!
//! * `GET  /object/<hash>` — return the serialized object (`<type> <size>\0…`).
//! * `HEAD /object/<hash>` — 200/404, no body: is it stored? (`put` prunes on it.)
//! * `POST /object/` — store the serialized object in the body, return its hash.
//!
//! Compute:
//!
//! * `GET /run?req=<hash>&trace=<id>` — run the ArgTree `<hash>` (`req` is the
//!   query param's historical name; its value is the ArgTree hash) and return
//!   the hash of its result, optionally emitting this invocation to an open
//!   trace stream.
//! * `POST /sub-run` — admit an exact detached child under an in-flight job's
//!   existing server-side run stack and secret store.
//! * `GET /trace/<id>/stream` — follow one live invocation as chunked NDJSON.
//!
//! The server runs no workers itself. Dispatch is pull-based (see
//! `design/runner-protocol.md`): runners long-poll `POST /runner/poll` with
//! their required args, the server matches pending `/run` jobs against the
//! parked polls, and the runner posts the job's `"<type> <hash>"` back via
//! `POST /runner/result` (or a `promise` the compute half resolves — see
//! `design/map-then.md`). The generic runner minting fresh worker containers is
//! `caos-runnerd`, an ordinary poller with no required args.
//!
//! Compute results are cached in Redis (`CAOS_REDIS_ADDR`, default
//! `caos-redis:6379`): the key is the arg-tree hash — the ArgTree itself, which
//! carries the worker image, std and salt — the value the
//! result hash. A hit skips the worker entirely. Redis is best-effort — if
//! it's unreachable we log and run uncached.
//!
//! Git transport:
//!
//! * `GET  /info/refs?service=…`, `POST /git-upload-pack`, `POST /git-receive-pack`
//!   — git smart-HTTP over the same repo, so the caos client can use the server as
//!   a `caos` git remote (push objects up, fetch refs/results down). See
//!   [`mod git`]; it delegates to `git http-backend`.
//!
//! The halves live in [`mod storage`], [`mod compute`], [`mod runner`], and
//! [`mod git`]; this file is the entry point, the shared [`Config`]/[`HttpError`],
//! and the request router.

mod compute;
mod git;
mod repair;
mod runner;
mod secrets;
mod storage;
mod trace;

use std::sync::Arc;

use tiny_http::{Method, Request, Response, Server, StatusCode};

/// Listen address; overridable for local runs outside the container. Binds the
/// IPv6 wildcard (dual-stack: also accepts IPv4) so runners can reach us over
/// IPv6-only networks too.
const DEFAULT_ADDR: &str = "[::]:80";

/// Where the git object database lives (the storage half, now in-process).
/// Override with `CAOS_GIT_DIR` (useful for local runs outside the container).
const DEFAULT_GIT_DIR: &str = "/git";

/// Registry base URL converted git-docker images are pushed to, reachable from
/// *this* container over the docker network. Override with
/// `CAOS_REGISTRY_PUSH_URL`.
const DEFAULT_REGISTRY_PUSH_URL: &str = "http://caos-registry:5000";

/// How the docker daemon that actually runs workers (runnerd's) refers to that
/// same registry — a published port on localhost, which docker treats as an
/// insecure registry, so no TLS/daemon config is needed. Override with
/// `CAOS_REGISTRY_PULL_HOST`.
const DEFAULT_REGISTRY_PULL_HOST: &str = "localhost:5000";

/// Redis (host:port) used to cache results. Override with `CAOS_REDIS_ADDR`.
const DEFAULT_REDIS_ADDR: &str = "caos-redis:6379";

/// Runtime configuration, read once from the environment at startup. Cloning
/// is cheap and lets admitted sub-runs outlive the request thread that launched
/// them while sharing the same repository and trace hub handles.
#[derive(Clone)]
struct Config {
    registry_push_url: String,
    registry_pull_host: String,
    redis_addr: String,
    /// Filesystem path to the git object database, passed to `git http-backend`
    /// as `GIT_PROJECT_ROOT` for the smart-HTTP transport (see [`mod git`]).
    git_dir: String,
    /// The git object database, served directly (storage is now in-process).
    /// Thread-safe: each request thread takes a local handle via `to_thread_local`.
    repo: gix::ThreadSafeRepository,
    trace: trace::Hub,
}

/// Install handlers so the process terminates on `SIGINT`/`SIGTERM`. This matters
/// in a container, where the daemon is PID 1: the kernel applies no default
/// disposition for these signals to PID 1, so without an explicit handler
/// `docker stop` (and `caosd down`'s Ctrl-C) would hang until the 10s `SIGKILL`.
fn install_termination_handlers() {
    // Async-signal-safe: we hold no state that needs flushing, so just exit.
    extern "C" fn terminate(_signum: std::ffi::c_int) {
        unsafe { exit_now(0) }
    }
    extern "C" {
        // libc, resolved against what std already links.
        fn signal(signum: std::ffi::c_int, handler: extern "C" fn(std::ffi::c_int)) -> usize;
        #[link_name = "_exit"]
        fn exit_now(code: std::ffi::c_int) -> !;
    }
    const SIGINT: std::ffi::c_int = 2;
    const SIGTERM: std::ffi::c_int = 15;
    unsafe {
        signal(SIGINT, terminate);
        signal(SIGTERM, terminate);
    }
}

fn main() {
    install_termination_handlers();

    let addr = std::env::var("SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let git_dir = env_or("CAOS_GIT_DIR", DEFAULT_GIT_DIR);

    // Self-bootstrap the bare repo on first run (e.g. a fresh fly Volume), the
    // same setup `caosd up` does by hand: `http.receivepack` lets clients
    // `git push`, `allowAnySHA1InWant` lets them fetch a result by bare hash.
    // `git init --bare` is idempotent, so this is a no-op once seeded.
    let git = |args: &[&str]| {
        run_required_git(args).unwrap_or_else(|error| {
            eprintln!("fatal: {error}");
            std::process::exit(1);
        });
    };
    if gix::open(&git_dir).is_err() {
        git(&["init", "-q", "--bare", &git_dir]);
    }
    // UNCONDITIONALLY, not just on a repo this process created. These two are
    // the server's own protocol requirements, so it owns them wherever the repo
    // came from — and a repo can arrive from anywhere: a test stack seeds one by
    // `git init --bare` plus a fetch of the deps it was handed
    // (test-stack/worker), and a plain `init` sets neither. That cost a suite
    // where every push came back `403` from the inner server, which reads as an
    // auth problem and is really a missing config.
    git(&["-C", &git_dir, "config", "http.receivepack", "true"]);
    git(&[
        "-C",
        &git_dir,
        "config",
        "uploadpack.allowAnySHA1InWant",
        "true",
    ]);
    // Request refs are transient negotiation anchors and result refs are a
    // server-written durability index. Neither belongs in repository-wide fetch
    // advertisements. Both remain visible to receive-pack negotiation: a client
    // can form its next request from a result object that exists only on the
    // server, so hiding that result makes send-pack try (and fail) to read it
    // locally.
    configure_ref_advertisements(&git_dir).unwrap_or_else(|error| {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    });
    remove_managed_pre_receive_hook(&git_dir).unwrap_or_else(|error| {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    });
    // Serve partial-clone (`--filter`) fetches: the std/merge worker fetches
    // the commit GRAPH only (`--filter=tree:0`) and lazily faults in just the
    // three trees `git merge-tree` needs, rather than the whole history's
    // trees and blobs. Same class of flag as the two above — a protocol
    // capability the server owns.
    git(&["-C", &git_dir, "config", "uploadpack.allowFilter", "true"]);
    // Never let git rewrite this repo's object store behind our back.
    // `git-receive-pack` (which http-backend spawns on every push) forks a
    // background repack; that rewrites the object store while a concurrent
    // `git-upload-pack` is streaming a fetch from it, which can truncate the
    // pack and surface on the client as the intermittent `fetch-pack: invalid
    // index-pack output`. Worse, we hold the same repo open as a
    // `gix::ThreadSafeRepository`: when its object database re-consolidates
    // against packs that moved under it, gix asserts ("if the generation
    // changed, the slot index must have changed for sure") and the panic
    // unwinds that request's thread. tiny_http then answers a bare 500 from
    // `Drop`, so the caller sees a body-less `500 Internal Server Error` and
    // nothing at all appears in this log.
    //
    // THREE settings, because one is no longer enough. `gc.auto 0` gates only
    // the `gc` task; git 2.54 added a `geometric-repack` maintenance task that
    // ignores every gc knob (`gc.auto`, `gc.autoPackLimit`,
    // `maintenance.gc.enabled` — all measured, all repack anyway). So also
    // refuse the auto-maintenance hook outright, and disable the task by name
    // for any path that reaches it another way.
    //
    // `--cruft` is why this is data loss and not just a crash: nearly every
    // object here is unreachable from a ref (results and trees are addressed
    // by hash), so a cruft repack sorts the CAS into a cruft pack and drops
    // whatever has not been touched in two weeks — while redis keeps handing
    // out result hashes that point at it.
    //
    // Unconditional, so an already-seeded repo is healed on the next restart
    // and not just a fresh one.
    git(&["-C", &git_dir, "config", "gc.auto", "0"]);
    git(&["-C", &git_dir, "config", "receive.autogc", "false"]);
    git(&[
        "-C",
        &git_dir,
        "config",
        "maintenance.geometric-repack.enabled",
        "false",
    ]);
    // Survive an unclean shutdown. git's DEFAULT is to publish a loose object or
    // a ref by renaming a temp file into place and never fsyncing it, so ext4
    // can journal the rename before the data lands and a crash leaves the right
    // filename holding zero bytes — which is not a lost object but a POISONED
    // one, because receive-pack validates every advertised ref and one
    // unreadable blob then rejects every push (see [`mod repair`]). `objects`
    // covers loose objects and packs, `reference` the ref files.
    //
    // `fsyncMethod=batch` is what makes this affordable on the loose-object side
    // — one hardware flush for a whole batch of writes rather than one each,
    // which matters when a single `caos put` lands thousands of objects. This is
    // git's own side only; the in-process gix writes are `storage`'s to sync.
    git(&["-C", &git_dir, "config", "core.fsync", "objects,reference"]);
    git(&["-C", &git_dir, "config", "core.fsyncMethod", "batch"]);
    // Keep previous values as a generic recovery path if a ref tip is damaged
    // despite the fsync policy above. `always` includes refs outside
    // refs/heads. Automatic GC is disabled above, so these logs do not expire
    // by themselves; design/chat.md records bounded retention/pruning as
    // follow-up work. Until that policy exists, retain recovery history rather
    // than inventing an expiry window that can silently remove the only sound
    // value for a damaged ref.
    git(&["-C", &git_dir, "config", "core.logAllRefUpdates", "always"]);

    // Clear what a previous crash left behind BEFORE opening the repo: gix
    // answers "is this object stored?" for a loose object with a path-exists
    // check, so an empty file has to be gone from disk before anything asks.
    let swept = repair::sweep_empty_loose_objects(&git_dir);

    // Open the object database once as a thread-safe handle; each request thread
    // takes a cheap local handle from it (see `handle`).
    let repo = match gix::open(&git_dir) {
        Ok(repo) => repo.into_sync(),
        Err(err) => {
            eprintln!("fatal: cannot open git repo at {git_dir}: {err}");
            std::process::exit(1);
        }
    };

    // Then the refs the sweep just orphaned, plus any that were already dangling.
    let dropped = repair::drop_broken_refs(&repo.to_thread_local(), &git_dir);
    if swept + dropped > 0 {
        eprintln!(
            "repair: {git_dir} had crash damage — dropped {swept} empty object(s) \
             and {dropped} broken ref(s)"
        );
    }

    // Shared read-only across handler threads (one per request, see below).
    let config = Arc::new(Config {
        registry_push_url: env_or("CAOS_REGISTRY_PUSH_URL", DEFAULT_REGISTRY_PUSH_URL),
        registry_pull_host: env_or("CAOS_REGISTRY_PULL_HOST", DEFAULT_REGISTRY_PULL_HOST),
        redis_addr: env_or("CAOS_REDIS_ADDR", DEFAULT_REDIS_ADDR),
        git_dir,
        repo,
        trace: trace::Hub::default(),
    });

    let server = match Server::http(addr.as_str()) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("fatal: cannot bind {addr}: {err}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "caos-server listening on http://{addr} (storage + compute), \
         git repo {}, registry push {} / pull {}, redis {}",
        config.git_dir, config.registry_push_url, config.registry_pull_host, config.redis_addr,
    );

    spawn_request_ref_pruner(config.git_dir.clone());

    // One thread per request, not a serial loop: a worker fetches its inputs
    // from `/object` while its own `/run` request is still being served, a
    // runner's poll parks for its whole TTL, and several top-level runs may be
    // in flight at once. Threads are cheap here: each mostly blocks (compute
    // fans out its own threads for parallel promise maps — see
    // compute::resolve_promise).
    for request in server.incoming_requests() {
        let config = Arc::clone(&config);
        std::thread::spawn(move || {
            if let Err(err) = handle(config, request) {
                // Only reachable if writing the response itself fails.
                eprintln!("failed to send response: {err}");
            }
        });
    }
}

/// Run one of the Git commands that establishes the server's storage
/// contract. Ignoring one of these failures lets the server start in a mode
/// where pushes, object fetches, or crash recovery are silently unsafe.
fn run_required_git(args: &[&str]) -> Result<(), String> {
    let command = format!(
        "git {}",
        args.iter()
            .map(|arg| format!("{arg:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let output = std::process::Command::new("git")
        .args(args)
        .output()
        .map_err(|error| format!("cannot run required command {command}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    Err(if detail.is_empty() {
        format!("required command {command} exited with {}", output.status)
    } else {
        format!(
            "required command {command} exited with {}: {detail}",
            output.status
        )
    })
}

fn configure_ref_advertisements(git_dir: &str) -> Result<(), String> {
    ensure_git_config_value(git_dir, "uploadpack.hideRefs", "refs/caos/req/")?;
    ensure_git_config_value(git_dir, "uploadpack.hideRefs", "refs/caos/res/")?;
    // Undo the setting written by the first exact-ref implementation. Server
    // repositories survive binary upgrades, so merely ceasing to add it would
    // leave existing installations unable to negotiate result objects.
    remove_git_config_value(git_dir, "receive.hideRefs", "refs/caos/res/")
}

/// Add one multi-valued Git setting once, preserving administrator-supplied
/// values and avoiding duplicate entries across server restarts.
fn ensure_git_config_value(git_dir: &str, key: &str, value: &str) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .args(["-C", git_dir, "config", "--get-all", key])
        .output()
        .map_err(|error| format!("reading git config {key}: {error}"))?;
    match output.status.code() {
        Some(0) => {
            let values = String::from_utf8(output.stdout)
                .map_err(|error| format!("git config {key} is not UTF-8: {error}"))?;
            if values.lines().any(|configured| configured == value) {
                return Ok(());
            }
        }
        Some(1) => {}
        _ => {
            return Err(format!(
                "reading git config {key}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    run_required_git(&["-C", git_dir, "config", "--add", key, value])
}

/// Remove one formerly server-owned multi-valued Git setting without touching
/// administrator-supplied values alongside it.
fn remove_git_config_value(git_dir: &str, key: &str, value: &str) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            git_dir,
            "config",
            "--fixed-value",
            "--unset-all",
            key,
            value,
        ])
        .output()
        .map_err(|error| format!("removing git config {key}: {error}"))?;
    match output.status.code() {
        Some(0) | Some(5) => Ok(()), // removed, or no exact value was present
        _ => Err(format!(
            "removing git config {key}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

const MANAGED_HOOK_MARKERS: [&str; 2] = [
    "# managed by caos-server: append-only refs",
    "# managed by caos-server: append-only conversation heads",
];

// TODO: Remove this one-time migration after every supported repository has
// been started by a server version that no longer installs the hook.
/// Remove the pre-receive hook installed by older CAOS servers.
///
/// The old hook execs this binary with a validator mode that no longer exists,
/// so merely ceasing to install it would make every push to an upgraded
/// repository fail. Its marker is the ownership proof: an unmarked hook and its
/// configured path belong to the administrator and are left untouched.
fn remove_managed_pre_receive_hook(git_dir: &str) -> Result<(), String> {
    let hooks = std::path::Path::new(git_dir).join("hooks");
    let hook = hooks.join("pre-receive");
    let hooks_value = hooks
        .to_str()
        .ok_or_else(|| format!("hooks path is not UTF-8: {}", hooks.display()))?;
    let contents = match std::fs::read(&hook) {
        Ok(contents) => contents,
        // A previous cleanup may have removed the hook without clearing the
        // absolute path our installer wrote. That exact value is redundant
        // with Git's default `<git-dir>/hooks`, so clearing it cannot disable a
        // surviving hook in this directory.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return remove_git_config_value(git_dir, "core.hooksPath", hooks_value);
        }
        Err(error) => return Err(format!("reading {}: {error}", hook.display())),
    };
    let managed = MANAGED_HOOK_MARKERS.iter().any(|marker| {
        contents
            .windows(marker.len())
            .any(|window| window == marker.as_bytes())
    });
    if !managed {
        return Ok(());
    }

    std::fs::remove_file(&hook).map_err(|error| format!("removing {}: {error}", hook.display()))?;
    // The installer always wrote this absolute value. Remove only that exact
    // value; preserve relative or alternate administrator-selected hook paths.
    remove_git_config_value(git_dir, "core.hooksPath", hooks_value)
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// An error that maps cleanly onto an HTTP status code + body.
pub(crate) struct HttpError {
    status: u16,
    message: String,
}

impl HttpError {
    pub(crate) fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// Plain-data accessors, for callers that clone an error across threads
    /// (single-flight broadcasts one outcome to every waiter).
    pub(crate) fn status(&self) -> u16 {
        self.status
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl From<std::io::Error> for HttpError {
    fn from(err: std::io::Error) -> Self {
        HttpError::new(500, format!("io error: {err}"))
    }
}

/// Dispatch a single request and send its response.
fn handle(config: Arc<Config>, mut request: Request) -> std::io::Result<()> {
    // Git smart-HTTP (the `caos` remote) is served by a separate CGI delegate that
    // sets its own status/headers, so it bypasses the `route` -> `from_data` path.
    let path = request.url().split('?').next().unwrap_or("").to_string();
    if git::is_git_path(&path) {
        return git::serve(&config, request);
    }
    // The WORLD guard (design/test-stack-image.md): a caos client built for
    // the other world must not drive this stack. The dangerous crossing is
    // silent — a host client against the test stack passes until the tree
    // under test changes the client, and then the suite is exercising host
    // code where the tested code was the point. Requests without the header
    // pass: git smart-HTTP returned above, and health probes are plain curl.
    if let Some(client) = request
        .headers()
        .iter()
        .find(|header| header.field.equiv(caos_world::WORLD_HEADER))
        .map(|header| header.value.as_str().to_string())
    {
        if client != caos_world::WORLD {
            return request.respond(
                Response::from_string(caos_world::mismatch(caos_world::WORLD, &client) + "\n")
                    .with_status_code(StatusCode(400)),
            );
        }
    }
    if request.method() == &Method::Get {
        if let Some(id) = path
            .strip_prefix("/trace/")
            .and_then(|rest| rest.strip_suffix("/stream"))
        {
            if !trace::valid_id(id) {
                return request.respond(
                    Response::from_string("invalid trace id\n").with_status_code(StatusCode(400)),
                );
            }
            if request.url().contains('?') {
                return request.respond(
                    Response::from_string("trace streams do not accept query parameters\n")
                        .with_status_code(StatusCode(400)),
                );
            }
            let stream = match config.trace.stream(id) {
                Ok(stream) => stream,
                Err(message) => {
                    return request.respond(
                        Response::from_string(format!("{message}\n"))
                            .with_status_code(StatusCode(409)),
                    )
                }
            };
            return stream.respond(request);
        }
    }

    match route(&config, &mut request) {
        Ok(body) => request.respond(Response::from_data(body)),
        Err(err) => request.respond(
            Response::from_string(format!("{}\n", err.message))
                .with_status_code(tiny_http::StatusCode(err.status)),
        ),
    }
}

/// Match the request to a handler and produce the response body. Serves the
/// storage endpoints (`/object*`), compute (`/run`, `/sub-run`), and the runner
/// protocol (`/runner/poll`, `/runner/result`).
fn route(config: &Arc<Config>, request: &mut Request) -> Result<Vec<u8>, HttpError> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };

    match request.method() {
        Method::Get if path == "/run" => {
            // The carried secrets store rides in a header (design/secrets.md),
            // out of band from the content-addressed ArgTree.
            let secrets_header = request
                .headers()
                .iter()
                .find(|h| h.field.equiv(secrets::HEADER))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            compute::run(config, &query, &secrets_header)
        }
        Method::Get if path == "/resolve-image" => compute::resolve_image_endpoint(config, &query),
        Method::Get => match path.strip_prefix("/object/") {
            Some(hash) if !hash.is_empty() => storage::get_object(config, hash),
            _ => Err(HttpError::new(404, "not found")),
        },
        Method::Head => match path.strip_prefix("/object/") {
            Some(hash) if !hash.is_empty() => storage::head_object(config, hash),
            _ => Err(HttpError::new(404, "not found")),
        },
        Method::Post if path == "/object/" || path == "/object" => {
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body)?;
            storage::post_object(config, &body)
        }
        Method::Post if path == "/sub-run" => {
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            runner::sub_run(&body)
        }
        Method::Post if path == "/runner/poll" || path == "/runner/result" => {
            let authorization = request
                .headers()
                .iter()
                .find(|h| h.field.to_string().eq_ignore_ascii_case("authorization"))
                .map(|h| h.value.to_string());
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            if path == "/runner/poll" {
                runner::poll(authorization.as_deref(), &body)
            } else {
                runner::result(authorization.as_deref(), &body)
            }
        }
        _ => Err(HttpError::new(404, "not found")),
    }
}

/// How often stale request refs are swept, and how long one is kept.
const REQ_REF_SWEEP: std::time::Duration = std::time::Duration::from_secs(120);
const REQ_REF_KEEP: std::time::Duration = std::time::Duration::from_secs(600);

/// Sweep old `refs/caos/req/<hash>` refs in the background.
///
/// EVERY PUSHED OBJECT LEAVES ONE (`ensure_pushed`), so they accumulate without
/// bound — a single suite run adds about 1300 — and every push and fetch after
/// that pays for them, because git advertises the whole ref list on each
/// connection. Measured on this repo: 2560 refs is a 251 KB advertisement and a
/// 258ms push of one small object; at 1231 refs the same push is 181ms. That is
/// roughly 58ms per thousand refs, on every one of the hundreds of pushes a
/// suite makes, and it gets worse every run.
///
/// Safe to delete, on two independent grounds. Nothing READS them: the name is
/// written by `ensure_pushed` and never looked up (a result is fetched by hash,
/// or by the `refs/caos/res/<argTree>` the server pins). And they anchor
/// nothing: GC is off here precisely because almost every object is unreachable
/// from any ref, so these are not what keeps objects alive.
///
/// AGE, not a flush. Their one job is to be a negotiation base for the NEXT
/// push from the same client, seconds later, so that an edited tree ships only
/// its delta. Deleting a fresh one would make that client re-send a closure it
/// already sent, which is the cost this is trying to avoid.
fn spawn_request_ref_pruner(git_dir: String) {
    std::thread::spawn(move || loop {
        std::thread::sleep(REQ_REF_SWEEP);
        let dir = std::path::Path::new(&git_dir).join("refs/caos/req");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // No directory yet (nothing pushed) is the ordinary case, not news.
            Err(_) => continue,
        };
        let mut stale = String::new();
        let mut count = 0usize;
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t.elapsed().map(|age| age > REQ_REF_KEEP).unwrap_or(false))
                .unwrap_or(false);
            if !old {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                stale.push_str(&format!("delete refs/caos/req/{name}\n"));
                count += 1;
            }
        }
        if count == 0 {
            continue;
        }
        // Through git rather than by unlinking: it takes the same locks the
        // push path does, and it copes with a ref that has been packed since.
        let mut child = match std::process::Command::new("git")
            .args(["-C", &git_dir, "update-ref", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                eprintln!("caos-server: pruning request refs: {e}");
                continue;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(stale.as_bytes());
        }
        match child.wait() {
            Ok(_) => eprintln!("caos-server: pruned {count} stale request ref(s)"),
            Err(e) => eprintln!("caos-server: pruning request refs: {e}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        configure_ref_advertisements, remove_managed_pre_receive_hook, run_required_git,
        MANAGED_HOOK_MARKERS,
    };
    use std::process::Command;

    #[test]
    fn required_git_command_reports_nonzero_status() {
        let absent = std::env::temp_dir().join(format!(
            "caos-required-git-test-{}-absent",
            std::process::id()
        ));
        std::fs::remove_dir_all(&absent).ok();
        let error = run_required_git(&[
            "-C",
            absent.to_str().unwrap(),
            "config",
            "http.receivepack",
            "true",
        ])
        .unwrap_err();
        assert!(error.contains("required command git"), "{error}");
        assert!(error.contains("exited with"), "{error}");
    }

    #[test]
    fn server_owned_refs_are_hidden_from_fetch_but_available_for_push_negotiation() {
        let dir = std::env::temp_dir().join(format!("caos-hidden-ref-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let git = |args: &[&str]| {
            let output = Command::new("git").args(args).output().unwrap();
            assert!(
                output.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        git(&["init", "-q", "--bare", dir.to_str().unwrap()]);
        git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "--add",
            "uploadpack.hideRefs",
            "refs/private/",
        ]);
        git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "--add",
            "receive.hideRefs",
            "refs/private-write/",
        ]);
        git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "--add",
            "receive.hideRefs",
            "refs/caos/res/",
        ]);
        let blob = git(&["-C", dir.to_str().unwrap(), "hash-object", "-w", "--stdin"]);
        let blob = blob.trim();
        for refname in [
            "refs/caos/req/request",
            "refs/caos/res/result",
            "refs/caos/v2/users/u-1/conversations/active/c-74657374",
        ] {
            git(&["-C", dir.to_str().unwrap(), "update-ref", refname, blob]);
        }
        configure_ref_advertisements(dir.to_str().unwrap()).unwrap();
        configure_ref_advertisements(dir.to_str().unwrap()).unwrap();
        let hidden = git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "--get-all",
            "uploadpack.hideRefs",
        ]);
        assert_eq!(hidden.lines().count(), 3, "{hidden}");
        assert!(hidden.lines().any(|value| value == "refs/private/"));
        let receive_hidden = git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "--get-all",
            "receive.hideRefs",
        ]);
        assert_eq!(receive_hidden.trim(), "refs/private-write/");

        let upload = git(&[
            "upload-pack",
            "--stateless-rpc",
            "--advertise-refs",
            dir.to_str().unwrap(),
        ]);
        assert!(!upload.contains("refs/caos/req/"));
        assert!(!upload.contains("refs/caos/res/"));
        assert!(upload.contains("refs/caos/v2/users/"));

        let receive = git(&[
            "receive-pack",
            "--stateless-rpc",
            "--advertise-refs",
            dir.to_str().unwrap(),
        ]);
        assert!(receive.contains("refs/caos/req/"));
        assert!(receive.contains("refs/caos/res/"));
        assert!(receive.contains("refs/caos/v2/users/"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn managed_pre_receive_hooks_are_removed_on_upgrade() {
        for (index, marker) in MANAGED_HOOK_MARKERS.iter().enumerate() {
            let dir = std::env::temp_dir().join(format!(
                "caos-managed-hook-test-{}-{index}",
                std::process::id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            run_required_git(&["init", "-q", "--bare", dir.to_str().unwrap()]).unwrap();
            let hooks = dir.join("hooks");
            let hook = hooks.join("pre-receive");
            std::fs::write(&hook, format!("#!/bin/sh\n{marker}\nexit 1\n")).unwrap();
            run_required_git(&[
                "-C",
                dir.to_str().unwrap(),
                "config",
                "core.hooksPath",
                hooks.to_str().unwrap(),
            ])
            .unwrap();

            remove_managed_pre_receive_hook(dir.to_str().unwrap()).unwrap();

            assert!(!hook.exists());
            let configured = Command::new("git")
                .args([
                    "-C",
                    dir.to_str().unwrap(),
                    "config",
                    "--get",
                    "core.hooksPath",
                ])
                .output()
                .unwrap();
            assert_eq!(configured.status.code(), Some(1));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn unmanaged_pre_receive_hook_and_path_are_preserved() {
        let dir =
            std::env::temp_dir().join(format!("caos-unmanaged-hook-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        run_required_git(&["init", "-q", "--bare", dir.to_str().unwrap()]).unwrap();
        let hooks = dir.join("hooks");
        let hook = hooks.join("pre-receive");
        let contents = b"#!/bin/sh\necho administrator hook\n";
        std::fs::write(&hook, contents).unwrap();
        run_required_git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "core.hooksPath",
            hooks.to_str().unwrap(),
        ])
        .unwrap();

        remove_managed_pre_receive_hook(dir.to_str().unwrap()).unwrap();

        assert_eq!(std::fs::read(&hook).unwrap(), contents);
        let configured = Command::new("git")
            .args([
                "-C",
                dir.to_str().unwrap(),
                "config",
                "--get",
                "core.hooksPath",
            ])
            .output()
            .unwrap();
        assert!(configured.status.success());
        assert_eq!(
            String::from_utf8(configured.stdout).unwrap().trim(),
            hooks.to_str().unwrap()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_managed_hook_path_is_removed_when_the_hook_is_already_absent() {
        let dir =
            std::env::temp_dir().join(format!("caos-stale-hook-path-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        run_required_git(&["init", "-q", "--bare", dir.to_str().unwrap()]).unwrap();
        let hooks = dir.join("hooks");
        run_required_git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "core.hooksPath",
            hooks.to_str().unwrap(),
        ])
        .unwrap();

        remove_managed_pre_receive_hook(dir.to_str().unwrap()).unwrap();

        let configured = Command::new("git")
            .args([
                "-C",
                dir.to_str().unwrap(),
                "config",
                "--get",
                "core.hooksPath",
            ])
            .output()
            .unwrap();
        assert_eq!(configured.status.code(), Some(1));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
