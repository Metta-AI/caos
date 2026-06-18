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
//!
//! Results are cached in Redis (`CAOS_REDIS_ADDR`, default `caos-redis:6379`):
//! the key is the image + args-tree hash, the value the result hash. A hit skips
//! the container entirely. Redis is best-effort — if it's unreachable we log and
//! run uncached.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

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

/// Redis (host:port) used to cache results. Override with `CAOS_REDIS_ADDR`.
const DEFAULT_REDIS_ADDR: &str = "caos-redis:6379";

/// How long to wait on Redis before giving up and running uncached.
const REDIS_TIMEOUT: Duration = Duration::from_secs(5);

/// The caos binary inside every compute image, forced as the entrypoint.
const CAOS_BIN: &str = "/bin/caos";

/// Runtime configuration, read once from the environment at startup.
struct Config {
    network: String,
    object_server_url: String,
    docker_bin: String,
    redis_addr: String,
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

    let addr = std::env::var("COMPUTE_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let config = Config {
        network: env_or("CAOS_DOCKER_NETWORK", DEFAULT_NETWORK),
        object_server_url: env_or("CAOS_OBJECT_SERVER_URL", DEFAULT_OBJECT_SERVER_URL),
        docker_bin: env_or("CAOS_DOCKER_BIN", DEFAULT_DOCKER_BIN),
        redis_addr: env_or("CAOS_REDIS_ADDR", DEFAULT_REDIS_ADDR),
    };

    let server = match Server::http(addr.as_str()) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("fatal: cannot bind {addr}: {err}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "compute-server listening on http://{addr}, network {}, object server {}, redis {}",
        config.network, config.object_server_url, config.redis_addr
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

    // Cache key is the image + args-tree hash; value is the result hash. Redis
    // is best-effort: a lookup/connection error just means we run uncached.
    let key = format!("caos:result:{image}\0{args}");
    match cache_get(&config.redis_addr, &key) {
        Ok(Some(result)) => {
            eprintln!("cache hit: image={image} args={args} -> {result}");
            return Ok(format!("{result}\n").into_bytes());
        }
        Ok(None) => eprintln!("cache miss: image={image} args={args}; running worker"),
        Err(e) => eprintln!("cache lookup failed ({e}); running worker: image={image} args={args}"),
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

    // Cache the result for next time (best-effort).
    match cache_set(&config.redis_addr, &key, &hash) {
        Ok(()) => eprintln!("ran worker: image={image} args={args} -> {hash} (cached)"),
        Err(e) => {
            eprintln!("ran worker: image={image} args={args} -> {hash} (cache store failed: {e})")
        }
    }

    Ok(format!("{hash}\n").into_bytes())
}

/// `GET key` from Redis, returning the value or None if the key is absent.
fn cache_get(addr: &str, key: &str) -> Result<Option<String>, String> {
    let mut stream = redis_connect(addr)?;
    stream
        .write_all(&resp_command(&["GET", key]))
        .map_err(|e| format!("write: {e}"))?;
    read_bulk_reply(&mut BufReader::new(stream))
}

/// `SET key value` in Redis.
fn cache_set(addr: &str, key: &str, value: &str) -> Result<(), String> {
    let mut stream = redis_connect(addr)?;
    stream
        .write_all(&resp_command(&["SET", key, value]))
        .map_err(|e| format!("write: {e}"))?;
    read_status_reply(&mut BufReader::new(stream))
}

/// Connect to Redis with read/write timeouts so a stuck server can't hang us.
fn redis_connect(addr: &str) -> Result<TcpStream, String> {
    let stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    let _ = stream.set_read_timeout(Some(REDIS_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REDIS_TIMEOUT));
    Ok(stream)
}

/// Encode a Redis command as a RESP array of bulk strings (binary-safe, so the
/// NUL in our cache key is fine).
fn resp_command(args: &[&str]) -> Vec<u8> {
    let mut buf = format!("*{}\r\n", args.len()).into_bytes();
    for arg in args {
        buf.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        buf.extend_from_slice(arg.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

/// Read a RESP bulk-string reply (`$<len>\r\n<data>\r\n`); a nil reply (`$-1`)
/// becomes None and an error reply (`-...`) becomes Err.
fn read_bulk_reply(reader: &mut impl BufRead) -> Result<Option<String>, String> {
    let header = read_reply_line(reader)?;
    match header.as_bytes().first() {
        Some(b'$') => {
            let len: i64 = header[1..]
                .parse()
                .map_err(|e| format!("bad bulk length: {e}"))?;
            if len < 0 {
                return Ok(None); // nil
            }
            let mut buf = vec![0u8; len as usize + 2]; // data + trailing CRLF
            reader
                .read_exact(&mut buf)
                .map_err(|e| format!("read: {e}"))?;
            buf.truncate(len as usize);
            String::from_utf8(buf)
                .map(Some)
                .map_err(|e| format!("non-utf8 value: {e}"))
        }
        Some(b'-') => Err(format!("redis error: {}", &header[1..])),
        _ => Err(format!("unexpected reply: {header:?}")),
    }
}

/// Read a RESP simple-status reply (`+OK\r\n`); an error reply becomes Err.
fn read_status_reply(reader: &mut impl BufRead) -> Result<(), String> {
    let header = read_reply_line(reader)?;
    match header.as_bytes().first() {
        Some(b'+') => Ok(()),
        Some(b'-') => Err(format!("redis error: {}", &header[1..])),
        _ => Err(format!("unexpected reply: {header:?}")),
    }
}

/// Read one CRLF-terminated reply line, without the trailing CRLF.
fn read_reply_line(reader: &mut impl BufRead) -> Result<String, String> {
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?
        == 0
    {
        return Err("redis closed the connection".to_string());
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
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
