//! object-server: a tiny HTTP front-end over a git object database.
//!
//! The git repository is mounted at `/git`. Two endpoints are served:
//!
//! * `GET  /object/<hash>` — return the raw (decompressed, header-stripped)
//!   data of the git object with that hash.
//! * `POST /object/?type=<blob|tree>` — write the request body into the repo as
//!   an object of that type (default `blob`) and return git's hash for it.
//!   Content-addressed, so it is idempotent.

use tiny_http::{Method, Request, Response, Server};

/// Where the git repository is mounted inside the container. Override with
/// `OBJECT_SERVER_GIT_DIR` (useful for local runs outside the container).
const GIT_DIR: &str = "/git";

/// Listen address; overridable for local runs outside the container.
const DEFAULT_ADDR: &str = "0.0.0.0:8080";

fn main() {
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
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));

    match request.method() {
        Method::Get => match path.strip_prefix("/object/") {
            Some(hash) if !hash.is_empty() => get_object(repo, hash),
            _ => Err(HttpError::new(404, "not found")),
        },
        Method::Post if path == "/object/" || path == "/object" => {
            let kind = query_param(query, "type").unwrap_or("blob");
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body)?;
            post_object(repo, kind, &body)
        }
        _ => Err(HttpError::new(404, "not found")),
    }
}

/// Find a `key=value` pair in a URL query string.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// `GET /object/<hash>` — return the object's decompressed content bytes.
fn get_object(repo: &gix::Repository, hash: &str) -> Result<Vec<u8>, HttpError> {
    let id = gix::ObjectId::from_hex(hash.as_bytes())
        .map_err(|err| HttpError::new(400, format!("invalid hash: {err}")))?;

    let object = repo
        .find_object(id)
        .map_err(|err| HttpError::new(404, format!("object not found: {err}")))?;

    Ok(object.data.clone())
}

/// `POST /object/?type=<blob|tree>` — write the body as an object of that type
/// and return its hash (hex + `\n`).
fn post_object(repo: &gix::Repository, kind: &str, body: &[u8]) -> Result<Vec<u8>, HttpError> {
    let id = match kind {
        "blob" => repo
            .write_blob(body)
            .map_err(|err| HttpError::new(500, format!("failed to write blob: {err}")))?
            .detach(),
        "tree" => {
            // Validate the canonical tree encoding before writing it as a real
            // tree object (so its hash is a genuine git tree hash).
            let tree = gix::objs::TreeRef::from_bytes(body, repo.object_hash())
                .map_err(|err| HttpError::new(400, format!("invalid tree: {err}")))?;
            repo.write_object(&tree)
                .map_err(|err| HttpError::new(500, format!("failed to write tree: {err}")))?
                .detach()
        }
        other => {
            return Err(HttpError::new(
                400,
                format!("unsupported object type: {other:?} (expected blob or tree)"),
            ))
        }
    };

    Ok(format!("{id}\n").into_bytes())
}
