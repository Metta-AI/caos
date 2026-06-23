//! caos server: storage and compute behind one endpoint.
//!
//! Storage (a tiny HTTP front-end over a git object database mounted at `/git`):
//!
//! * `GET  /object/<hash>` — return the serialized object (`<type> <size>\0…`).
//! * `POST /object/` — store the serialized object in the body, return its hash.
//!
//! Compute:
//!
//! * `GET /run?image=<image>&args=<hash>` — run `<image>` over the args tree
//!   `<hash>` and return the hash of its result.
//!
//! `/run` shells out to the `docker` CLI:
//!
//! ```text
//! docker run --rm --network <net> -e CAOS_SERVER_URL=<url> \
//!     --entrypoint /bin/caos <image> entrypoint --args=<hash>
//! ```
//!
//! Forcing `--entrypoint /bin/caos` means any image carrying the `caos` binary
//! and a `/worker` works as a compute image, regardless of its own configured
//! entrypoint/command. `caos entrypoint` populates `/cas/args` from `<hash>`,
//! runs `/worker`, and prints the hash recorded at `/cas/out` on its stdout —
//! which `docker run` forwards to ours, so the container's stdout *is* the
//! result hash. We return it as the response body.
//!
//! The server's own URL (`CAOS_SERVER_URL`) is injected so the worker can reach
//! storage and — for a worker that itself calls `caos run`, like the fold worker
//! — call back into compute. The container reaches us over the Docker network, so
//! it must be the same network the server runs on (default `caos-net`).
//!
//! Compute results are cached in Redis (`CAOS_REDIS_ADDR`, default
//! `caos-redis:6379`): the key is the image + args-tree hash, the value the
//! result hash. A hit skips the container entirely. Redis is best-effort — if
//! it's unreachable we log and run uncached.
//!
//! The two halves live in [`mod storage`] and [`mod compute`]; this file is the
//! entry point, the shared [`Config`]/[`HttpError`], and the request router.

mod compute;
mod storage;

use std::sync::Arc;

use tiny_http::{Method, Request, Response, Server};

/// Listen address; overridable for local runs outside the container.
const DEFAULT_ADDR: &str = "0.0.0.0:80";

/// Docker network the worker container joins, so it resolves the server by name.
/// Override with `CAOS_DOCKER_NETWORK`.
const DEFAULT_NETWORK: &str = "caos-net";

/// This server's URL as seen from inside the Docker network, passed into each
/// worker container — for both storage (`caos get`/`put`) and compute (a worker
/// that calls `caos run`, e.g. the fold worker, which recurses). Override with
/// `CAOS_SERVER_URL`.
const DEFAULT_SERVER_URL: &str = "http://caos-server";

/// Where the git object database lives (the storage half, now in-process).
/// Override with `CAOS_GIT_DIR` (useful for local runs outside the container).
const DEFAULT_GIT_DIR: &str = "/git";

/// Registry base URL converted git-docker images are pushed to, reachable from
/// *this* container over the docker network. Override with
/// `CAOS_REGISTRY_PUSH_URL`.
const DEFAULT_REGISTRY_PUSH_URL: &str = "http://caos-registry:5000";

/// How the host's docker daemon (which actually runs the worker) refers to that
/// same registry — a published port on localhost, which docker treats as an
/// insecure registry, so no TLS/daemon config is needed. Override with
/// `CAOS_REGISTRY_PULL_HOST`.
const DEFAULT_REGISTRY_PULL_HOST: &str = "localhost:5000";

/// `docker` binary to invoke. Override with `CAOS_DOCKER_BIN`.
const DEFAULT_DOCKER_BIN: &str = "docker";

/// Redis (host:port) used to cache results. Override with `CAOS_REDIS_ADDR`.
const DEFAULT_REDIS_ADDR: &str = "caos-redis:6379";

/// Runtime configuration, read once from the environment at startup.
struct Config {
    network: String,
    server_url: String,
    registry_push_url: String,
    registry_pull_host: String,
    docker_bin: String,
    redis_addr: String,
    /// The git object database, served directly (storage is now in-process).
    /// Thread-safe: each request thread takes a local handle via `to_thread_local`.
    repo: gix::ThreadSafeRepository,
}

/// Install handlers so the process terminates on `SIGINT`/`SIGTERM`. This matters
/// in a container, where the daemon is PID 1: the kernel applies no default
/// disposition for these signals to PID 1, so without an explicit handler
/// `docker stop` (and Tilt's Ctrl-C) would hang until the 10s `SIGKILL`.
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
        network: env_or("CAOS_DOCKER_NETWORK", DEFAULT_NETWORK),
        server_url: env_or("CAOS_SERVER_URL", DEFAULT_SERVER_URL),
        registry_push_url: env_or("CAOS_REGISTRY_PUSH_URL", DEFAULT_REGISTRY_PUSH_URL),
        registry_pull_host: env_or("CAOS_REGISTRY_PULL_HOST", DEFAULT_REGISTRY_PULL_HOST),
        docker_bin: env_or("CAOS_DOCKER_BIN", DEFAULT_DOCKER_BIN),
        redis_addr: env_or("CAOS_REDIS_ADDR", DEFAULT_REDIS_ADDR),
        repo,
    });

    let server = match Server::http(addr.as_str()) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("fatal: cannot bind {addr}: {err}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "caos-server listening on http://{addr} (storage + compute), network {}, \
         git repo {git_dir}, url {}, registry push {} / pull {}, redis {}",
        config.network,
        config.server_url,
        config.registry_push_url,
        config.registry_pull_host,
        config.redis_addr
    );

    // One thread per request, not a serial loop: a worker can itself call back
    // into us (the fold worker recurses via `caos run`), and that nested request
    // must be served while its parent's request is still blocked waiting on the
    // `docker run` it spawned. A serial loop — or any pool smaller than the tree
    // is deep — would deadlock. Threads are cheap here: each just blocks in
    // `docker run`'s `waitpid`.
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
}

impl From<std::io::Error> for HttpError {
    fn from(err: std::io::Error) -> Self {
        HttpError::new(500, format!("io error: {err}"))
    }
}

/// Dispatch a single request and send its response.
fn handle(config: &Config, mut request: Request) -> std::io::Result<()> {
    match route(config, &mut request) {
        Ok(body) => request.respond(Response::from_data(body)),
        Err(err) => request.respond(
            Response::from_string(format!("{}\n", err.message))
                .with_status_code(tiny_http::StatusCode(err.status)),
        ),
    }
}

/// Match the request to a handler and produce the response body. Serves both the
/// storage endpoints (`/object*`) and compute (`/run`).
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
        Method::Post if path == "/object/" || path == "/object" => {
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body)?;
            storage::post_object(config, &body)
        }
        _ => Err(HttpError::new(404, "not found")),
    }
}
