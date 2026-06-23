//! Git smart-HTTP: serve the server's object database to ordinary `git` clients.
//!
//! This is the transport the caos client speaks to as the `caos` remote: it
//! `git push`es objects up (upload new data) and `git fetch`es refs and results
//! back down. We don't reimplement the pack protocol — we delegate to git's own
//! `git http-backend` CGI, which already speaks both halves correctly.
//!
//! It's additive: the three smart-HTTP paths below don't collide with `/object`
//! or `/run`, so the existing HTTP storage/compute endpoints are untouched.
//!
//! The smart protocol uses exactly these requests, all relative to the remote's
//! URL (so the caos remote is just the bare server URL, e.g. `http://caos-server`):
//!
//! ```text
//! GET  /info/refs?service=git-upload-pack     (fetch: ref advertisement)
//! POST /git-upload-pack                        (fetch: pack negotiation)
//! GET  /info/refs?service=git-receive-pack    (push:  ref advertisement)
//! POST /git-receive-pack                       (push:  the packfile)
//! ```
//!
//! `git http-backend` is a CGI program: it reads the request via meta-variables
//! in the environment (`REQUEST_METHOD`, `PATH_INFO`, `QUERY_STRING`,
//! `CONTENT_TYPE`, …) plus the request body on stdin, and writes a CGI response
//! on stdout — a few headers (notably `Status:`), a blank line, then the body.
//! We translate our [`Request`] into that environment, feed the body, and parse
//! its stdout back into a [`Response`].
//!
//! Receive-pack (push) is only honoured when the served repo has
//! `http.receivepack=true`; the current mounted repo doesn't, so push is rejected
//! for now — fetch round-trips work, which is all this slice validates.

use std::io::Write;
use std::process::{Command, Stdio};

use tiny_http::{Header, Request, Response, StatusCode};

use crate::Config;

/// True if `path` is one of the git smart-HTTP service paths we serve. Routed
/// ahead of the `/object` + `/run` endpoints, which use disjoint paths.
pub(crate) fn is_git_path(path: &str) -> bool {
    matches!(path, "/info/refs" | "/git-upload-pack" | "/git-receive-pack")
}

/// Serve a git smart-HTTP request by delegating to `git http-backend` (CGI), then
/// translating its CGI response back into an HTTP one. Consumes the request: it
/// reads the body and sends the response itself.
pub(crate) fn serve(config: &Config, mut request: Request) -> std::io::Result<()> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };
    let method = request.method().as_str().to_string();

    // Forward the two headers http-backend needs as CGI meta-variables.
    let (mut content_type, mut content_length) = (String::new(), String::new());
    for header in request.headers() {
        match header.field.as_str().as_str().to_ascii_lowercase().as_str() {
            "content-type" => content_type = header.value.as_str().to_string(),
            "content-length" => content_length = header.value.as_str().to_string(),
            _ => {}
        }
    }

    // The request body: empty for the GET advertisements, the negotiation/packfile
    // for the POSTs.
    let mut body = Vec::new();
    request.as_reader().read_to_end(&mut body)?;

    // GIT_PROJECT_ROOT is the repo itself: with PATH_INFO carrying only the
    // service suffix (`/info/refs`, …), the repo path before it is empty, so
    // http-backend resolves the repo to GIT_PROJECT_ROOT directly.
    // GIT_HTTP_EXPORT_ALL exports it without a `git-daemon-export-ok` marker.
    let mut child = Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", &config.git_dir)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", &method)
        .env("PATH_INFO", &path)
        .env("QUERY_STRING", &query)
        .env("CONTENT_TYPE", &content_type)
        .env("CONTENT_LENGTH", &content_length)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    // Write stdin from a separate thread: http-backend streams stdout as it reads
    // stdin (a large clone fills stdout long before we'd finish a serial write),
    // so feeding the body and draining stdout must run concurrently or deadlock.
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&body);
        // Dropping stdin closes the pipe, signalling EOF to http-backend.
    });
    let output = child.wait_with_output()?;
    let _ = writer.join();

    let response = cgi_to_response(&output.stdout);
    request.respond(response)
}

/// Parse a CGI response (`Header: value` lines, a blank line, then the body) into
/// an HTTP [`Response`]. The `Status:` header sets the status code (default 200);
/// every other header is forwarded as-is.
fn cgi_to_response(out: &[u8]) -> Response<std::io::Cursor<Vec<u8>>> {
    let (head, body) = split_headers(out);
    let head = String::from_utf8_lossy(head);

    let mut status = 200u16;
    let mut headers = Vec::new();
    for line in head.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            // e.g. "404 Not Found" — take the leading code.
            status = value
                .split_whitespace()
                .next()
                .and_then(|c| c.parse().ok())
                .unwrap_or(200);
        } else if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            headers.push(header);
        }
    }

    let mut response = Response::from_data(body.to_vec()).with_status_code(StatusCode(status));
    for header in headers {
        response.add_header(header);
    }
    response
}

/// Split a CGI payload into its header block and body at the first blank line,
/// accepting either `\r\n\r\n` or `\n\n` as the separator. If there's no blank
/// line at all, treat the whole thing as headers (an empty body).
fn split_headers(out: &[u8]) -> (&[u8], &[u8]) {
    if let Some(pos) = find(out, b"\r\n\r\n") {
        (&out[..pos], &out[pos + 4..])
    } else if let Some(pos) = find(out, b"\n\n") {
        (&out[..pos], &out[pos + 2..])
    } else {
        (out, &[])
    }
}

/// First index of `needle` in `haystack`, if present.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
