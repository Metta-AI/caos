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

use std::io::Read;
use std::process::{Command, Stdio};

use tiny_http::{Header, Request, Response, StatusCode};

use crate::Config;

/// True if `path` is one of the git smart-HTTP service paths we serve. Routed
/// ahead of the `/object` + `/run` endpoints, which use disjoint paths.
pub(crate) fn is_git_path(path: &str) -> bool {
    matches!(
        path,
        "/info/refs" | "/git-upload-pack" | "/git-receive-pack"
    )
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

    // Forward the headers http-backend needs as CGI meta-variables. Content-Type
    // and Content-Length are the obvious two; Content-Encoding matters because git
    // gzip-compresses a large request body (e.g. the many `have` lines a big repo
    // sends during fetch negotiation, or a sizeable push) and sets
    // `Content-Encoding: gzip`. http-backend only inflates such a body when it sees
    // the `HTTP_CONTENT_ENCODING` CGI variable — without it, it reads the gzip
    // bytes as pkt-lines and dies with "bad line length character", which the
    // client sees as "the remote end hung up unexpectedly". A small repo's request
    // is never gzipped, so this stayed latent until a real (large) repo hit it.
    let (mut content_type, mut content_length, mut content_encoding) =
        (String::new(), String::new(), String::new());
    for header in request.headers() {
        match header.field.as_str().as_str().to_ascii_lowercase().as_str() {
            "content-type" => content_type = header.value.as_str().to_string(),
            "content-length" => content_length = header.value.as_str().to_string(),
            "content-encoding" => content_encoding = header.value.as_str().to_string(),
            _ => {}
        }
    }

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
        .env("HTTP_CONTENT_ENCODING", &content_encoding)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    // Feed the request body into http-backend's stdin in constant memory, then
    // signal EOF by dropping stdin. git's smart-HTTP services consume their whole
    // input before producing output (upload-pack reads the wants, then writes the
    // pack; receive-pack reads the pack, then writes a short report), so feeding
    // stdin fully before we read stdout can't deadlock.
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let _ = std::io::copy(request.as_reader(), &mut stdin);
    drop(stdin);

    // Parse only the CGI header block off the front of stdout; everything past the
    // blank line is the response body, which we hand to tiny_http as a *reader* so
    // it streams straight to the client. A fetch's packfile can be multiple GB
    // (e.g. the `rustc` image's std closure) — buffering it whole here OOM-killed
    // the server. `data_length = None` makes tiny_http chunk the response, which
    // git's HTTP clients accept.
    let (status, headers, leftover) = read_cgi_headers(&mut stdout)?;
    let body = std::io::Cursor::new(leftover).chain(stdout);
    let result = request.respond(Response::new(StatusCode(status), headers, body, None, None));
    let _ = child.wait();
    result
}

/// Read just the CGI header block from the front of `stdout` — up to the first
/// blank line (`\r\n\r\n` or `\n\n`) — returning the status code, the forwarded
/// headers, and any bytes already read past the separator (the start of the
/// response body). Reads in small chunks so the multi-GB body that follows is
/// never pulled into memory here. A header block over 64 KiB (or EOF with no
/// separator) is treated as the whole response, with an empty body.
fn read_cgi_headers(stdout: &mut impl Read) -> std::io::Result<(u16, Vec<Header>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let split = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break Some((pos, 4));
        }
        if let Some(pos) = find(&buf, b"\n\n") {
            break Some((pos, 2));
        }
        if buf.len() > 64 * 1024 {
            break None;
        }
        let n = stdout.read(&mut chunk)?;
        if n == 0 {
            break None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let (head, leftover) = match split {
        Some((pos, len)) => (&buf[..pos], buf[pos + len..].to_vec()),
        None => (&buf[..], Vec::new()),
    };
    let (status, headers) = parse_cgi_head(head);
    Ok((status, headers, leftover))
}

/// Parse a CGI header block (`Header: value` lines). `Status:` sets the HTTP
/// status (default 200); `Content-Length` is dropped (we stream with chunked
/// encoding, so a stale length would conflict); every other header is forwarded.
fn parse_cgi_head(head: &[u8]) -> (u16, Vec<Header>) {
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
        } else if name.eq_ignore_ascii_case("content-length") {
            continue;
        } else if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            headers.push(header);
        }
    }
    (status, headers)
}

/// First index of `needle` in `haystack`, if present.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
