//! compute-server: run a containerized compute step and return its result hash.
//!
//! One endpoint:
//!
//! * `GET /run?image=<image>&args=<hash>` — run `<image>` over the args tree
//!   `<hash>` and return the hash of its result.
//!
//! It shells out to the `docker` CLI:
//!
//! ```text
//! docker run --rm --network <net> -e CAOS_OBJECT_SERVER_URL=<url> \
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
//! The container reaches the object server over the Docker network, so it must
//! be the same network the object server runs on (default `caos-net`).

use std::process::Command;

use tiny_http::{Method, Request, Response, Server};

/// Listen address; overridable for local runs outside the container.
const DEFAULT_ADDR: &str = "0.0.0.0:9090";

/// Docker network the worker container joins, so the object server resolves by
/// name. Override with `CAOS_DOCKER_NETWORK`.
const DEFAULT_NETWORK: &str = "caos-net";

/// Object-server URL passed into the worker container. Override with
/// `CAOS_OBJECT_SERVER_URL`.
const DEFAULT_OBJECT_SERVER_URL: &str = "http://caos-object-server:8080";

/// `docker` binary to invoke. Override with `CAOS_DOCKER_BIN`.
const DEFAULT_DOCKER_BIN: &str = "docker";

/// The caos binary inside every compute image, forced as the entrypoint.
const CAOS_BIN: &str = "/bin/caos";

/// Runtime configuration, read once from the environment at startup.
struct Config {
    network: String,
    object_server_url: String,
    docker_bin: String,
}

fn main() {
    let addr = std::env::var("COMPUTE_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let config = Config {
        network: env_or("CAOS_DOCKER_NETWORK", DEFAULT_NETWORK),
        object_server_url: env_or("CAOS_OBJECT_SERVER_URL", DEFAULT_OBJECT_SERVER_URL),
        docker_bin: env_or("CAOS_DOCKER_BIN", DEFAULT_DOCKER_BIN),
    };

    let server = match Server::http(addr.as_str()) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("fatal: cannot bind {addr}: {err}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "compute-server listening on http://{addr}, network {}, object server {}",
        config.network, config.object_server_url
    );

    for request in server.incoming_requests() {
        if let Err(err) = handle(&config, request) {
            // Only reachable if writing the response itself fails.
            eprintln!("failed to send response: {err}");
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// An error that maps cleanly onto an HTTP status code + body.
struct HttpError {
    status: u16,
    message: String,
}

impl HttpError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

/// Dispatch a single request and send its response.
fn handle(config: &Config, request: Request) -> std::io::Result<()> {
    match route(config, &request) {
        Ok(body) => request.respond(Response::from_data(body)),
        Err(err) => request.respond(
            Response::from_string(format!("{}\n", err.message))
                .with_status_code(tiny_http::StatusCode(err.status)),
        ),
    }
}

/// Match the request to a handler and produce the response body.
fn route(config: &Config, request: &Request) -> Result<Vec<u8>, HttpError> {
    let url = request.url();
    let (path, query) = url.split_once('?').unwrap_or((url, ""));

    match (request.method(), path) {
        (Method::Get, "/run") => run(config, query),
        _ => Err(HttpError::new(404, "not found")),
    }
}

/// `GET /run?image=<image>&args=<hash>` — run the image and return its result.
fn run(config: &Config, query: &str) -> Result<Vec<u8>, HttpError> {
    let image = query_param(query, "image")
        .ok_or_else(|| HttpError::new(400, "missing 'image' query parameter"))?;
    let args = query_param(query, "args")
        .ok_or_else(|| HttpError::new(400, "missing 'args' query parameter"))?;

    // The image becomes a positional `docker run` argument; reject a leading '-'
    // so it can't be misread as a flag. The args hash is interpolated into
    // `--args=`; require it to be a plain hex object id.
    if image.is_empty() || image.starts_with('-') {
        return Err(HttpError::new(400, format!("invalid image: {image:?}")));
    }
    if args.is_empty() || !args.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HttpError::new(400, format!("invalid args hash: {args:?}")));
    }

    let output = Command::new(&config.docker_bin)
        .arg("run")
        .arg("--rm")
        .args(["--network", &config.network])
        .args([
            "-e",
            &format!("CAOS_OBJECT_SERVER_URL={}", config.object_server_url),
        ])
        .args(["--entrypoint", CAOS_BIN])
        .arg(&image)
        .arg("entrypoint")
        .arg(format!("--args={args}"))
        .output()
        .map_err(|e| HttpError::new(500, format!("running {}: {e}", config.docker_bin)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(HttpError::new(
            500,
            format!("worker container failed ({}):\n{stderr}", output.status),
        ));
    }

    // The container's stdout is the result hash printed by `caos entrypoint`.
    let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if hash.is_empty() {
        return Err(HttpError::new(
            500,
            "worker container produced no result hash on stdout",
        ));
    }
    Ok(format!("{hash}\n").into_bytes())
}

/// Find `name` in an `a=b&c=d` query string and percent-decode its value.
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| percent_decode(v))
    })
}

/// Percent-decode a URL component. `%XX` becomes its byte; `+` is left as-is
/// (we never encode spaces as `+`). Invalid escapes are passed through verbatim.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // `%XX` (two hex digits) decodes to one byte; anything else passes through.
        if bytes[i] == b'%' {
            if let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|b| hex_val(*b)),
                bytes.get(i + 2).and_then(|b| hex_val(*b)),
            ) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Value of a single hex digit, or `None` if it isn't one.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
