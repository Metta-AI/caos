//! object-server: a tiny HTTP front-end over a git object database.
//!
//! The git repository is mounted at `/git`. Two endpoints are served:
//!
//! Objects cross the wire in git's native serialized form,
//! `<type> <size>\0<content>` (uncompressed):
//!
//! * `GET  /object/<hash>` — return the serialized object with that hash.
//! * `POST /object/` — store the serialized object in the body (its type comes
//!   from the header) and return git's hash for it. Content-addressed, so it is
//!   idempotent.

use tiny_http::{Method, Request, Response, Server};

/// Where the git repository is mounted inside the container. Override with
/// `OBJECT_SERVER_GIT_DIR` (useful for local runs outside the container).
const GIT_DIR: &str = "/git";

/// Listen address; overridable for local runs outside the container.
const DEFAULT_ADDR: &str = "0.0.0.0:80";

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

    let addr = std::env::var("OBJECT_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let git_dir = std::env::var("OBJECT_SERVER_GIT_DIR").unwrap_or_else(|_| GIT_DIR.to_string());

    // Open the repo once and reuse it. Requests are served sequentially by the
    // loop below, so a single (non-Send) handle is all we need.
    let repo = match gix::open(&git_dir) {
        Ok(repo) => repo,
        Err(err) => {
            eprintln!("fatal: cannot open git repo at {git_dir}: {err}");
            std::process::exit(1);
        }
    };

    let server = match Server::http(addr.as_str()) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("fatal: cannot bind {addr}: {err}");
            std::process::exit(1);
        }
    };
    eprintln!("object-server listening on http://{addr}, git repo at {git_dir}");

    for request in server.incoming_requests() {
        if let Err(err) = handle(&repo, request) {
            // Only reachable if writing the response itself fails.
            eprintln!("failed to send response: {err}");
        }
    }
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

impl From<std::io::Error> for HttpError {
    fn from(err: std::io::Error) -> Self {
        HttpError::new(500, format!("io error: {err}"))
    }
}

/// Dispatch a single request and send its response.
fn handle(repo: &gix::Repository, mut request: Request) -> std::io::Result<()> {
    match route(repo, &mut request) {
        Ok(body) => request.respond(Response::from_data(body)),
        Err(err) => request.respond(
            Response::from_string(format!("{}\n", err.message))
                .with_status_code(tiny_http::StatusCode(err.status)),
        ),
    }
}

/// Match the request to a handler and produce the response body.
fn route(repo: &gix::Repository, request: &mut Request) -> Result<Vec<u8>, HttpError> {
    let path = request.url().to_string();

    match request.method() {
        Method::Get => match path.strip_prefix("/object/") {
            Some(hash) if !hash.is_empty() => get_object(repo, hash),
            _ => Err(HttpError::new(404, "not found")),
        },
        Method::Post if path == "/object/" || path == "/object" => {
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body)?;
            post_object(repo, &body)
        }
        _ => Err(HttpError::new(404, "not found")),
    }
}

/// `GET /object/<hash>` — return the serialized object: git's native
/// `<type> <size>\0<content>` form (uncompressed).
fn get_object(repo: &gix::Repository, hash: &str) -> Result<Vec<u8>, HttpError> {
    let id = gix::ObjectId::from_hex(hash.as_bytes())
        .map_err(|err| HttpError::new(400, format!("invalid hash: {err}")))?;

    let object = repo
        .find_object(id)
        .map_err(|err| HttpError::new(404, format!("object not found: {err}")))?;

    let mut out = object_header(object.kind, object.data.len());
    out.extend_from_slice(&object.data);
    Ok(out)
}

/// `POST /object/` — store a serialized object (`<type> <size>\0<content>`) and
/// return its hash (hex + `\n`). The type and size come from the body's header.
fn post_object(repo: &gix::Repository, body: &[u8]) -> Result<Vec<u8>, HttpError> {
    let (kind, content) = parse_object(body)?;

    let id = match kind {
        gix::object::Kind::Blob => repo
            .write_blob(content)
            .map_err(|err| HttpError::new(500, format!("failed to write blob: {err}")))?
            .detach(),
        gix::object::Kind::Tree => {
            // Validate the canonical tree encoding before writing it as a real
            // tree object (so its hash is a genuine git tree hash).
            let tree = gix::objs::TreeRef::from_bytes(content, repo.object_hash())
                .map_err(|err| HttpError::new(400, format!("invalid tree: {err}")))?;
            repo.write_object(&tree)
                .map_err(|err| HttpError::new(500, format!("failed to write tree: {err}")))?
                .detach()
        }
        other => {
            return Err(HttpError::new(
                400,
                format!("unsupported object type: {other} (expected blob or tree)"),
            ))
        }
    };

    Ok(format!("{id}\n").into_bytes())
}

/// Build a git object header: `<type> <size>\0`.
fn object_header(kind: gix::object::Kind, size: usize) -> Vec<u8> {
    format!("{kind} {size}\0").into_bytes()
}

/// Split a serialized object into its type and content, validating the header.
fn parse_object(body: &[u8]) -> Result<(gix::object::Kind, &[u8]), HttpError> {
    let nul = body
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| HttpError::new(400, "malformed object: missing NUL after header"))?;
    let header = std::str::from_utf8(&body[..nul])
        .map_err(|_| HttpError::new(400, "malformed object header"))?;
    let content = &body[nul + 1..];

    let (kind, size) = header
        .split_once(' ')
        .ok_or_else(|| HttpError::new(400, "malformed object header: expected '<type> <size>'"))?;
    let size: usize = size
        .parse()
        .map_err(|_| HttpError::new(400, format!("malformed object size: {size:?}")))?;
    if size != content.len() {
        return Err(HttpError::new(
            400,
            format!("object size {size} != content length {}", content.len()),
        ));
    }
    let kind = gix::object::Kind::from_bytes(kind.as_bytes())
        .map_err(|_| HttpError::new(400, format!("unknown object type: {kind:?}")))?;
    Ok((kind, content))
}
