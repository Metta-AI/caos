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
//! * `GET /run?req=<hash>&trace=<id>` — run the request tree `<hash>` and return
//!   the hash of its result, optionally emitting this invocation to an open
//!   trace stream.
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
//! `caos-redis:6379`): the key is the request hash (the args tree — which carries
//! the worker image — plus std and salt), the value the
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
mod runner;
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

/// Runtime configuration, read once from the environment at startup.
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
        let _ = std::process::Command::new("git").args(args).status();
    };
    if gix::open(&git_dir).is_err() {
        git(&["init", "-q", "--bare", &git_dir]);
    }
    // UNCONDITIONALLY, not just on a repo this process created. These two are
    // the server's own protocol requirements, so it owns them wherever the repo
    // came from — and a repo can arrive from anywhere: a test stack seeds one by
    // `git init --bare` plus a fetch of the std it was handed
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

    // Open the object database once as a thread-safe handle; each request thread
    // takes a cheap local handle from it (see `handle`).
    let repo = match gix::open(&git_dir) {
        Ok(repo) => repo.into_sync(),
        Err(err) => {
            eprintln!("fatal: cannot open git repo at {git_dir}: {err}");
            std::process::exit(1);
        }
    };

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

    // One thread per request, not a serial loop: a worker fetches its inputs
    // from `/object` while its own `/run` request is still being served, a
    // runner's poll parks for its whole TTL, and several top-level runs may be
    // in flight at once. Threads are cheap here: each mostly blocks (compute
    // fans out its own threads for parallel promise maps — see
    // compute::resolve_promise).
    for request in server.incoming_requests() {
        let config = Arc::clone(&config);
        std::thread::spawn(move || {
            if let Err(err) = handle(&config, request) {
                // Only reachable if writing the response itself fails.
                eprintln!("failed to send response: {err}");
            }
        });
    }
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
fn handle(config: &Config, mut request: Request) -> std::io::Result<()> {
    // Git smart-HTTP (the `caos` remote) is served by a separate CGI delegate that
    // sets its own status/headers, so it bypasses the `route` -> `from_data` path.
    let path = request.url().split('?').next().unwrap_or("").to_string();
    if git::is_git_path(&path) {
        return git::serve(config, request);
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

    match route(config, &mut request) {
        Ok(body) => request.respond(Response::from_data(body)),
        Err(err) => request.respond(
            Response::from_string(format!("{}\n", err.message))
                .with_status_code(tiny_http::StatusCode(err.status)),
        ),
    }
}

/// Match the request to a handler and produce the response body. Serves the
/// storage endpoints (`/object*`), compute (`/run`), and the runner protocol
/// (`/runner/poll`, `/runner/result`).
fn route(config: &Config, request: &mut Request) -> Result<Vec<u8>, HttpError> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };

    match request.method() {
        Method::Get if path == "/run" => compute::run(config, &query),
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
