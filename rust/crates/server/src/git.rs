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
//! Pushes must supply complete history. Reject shallow boundaries before Git
//! handles the pack: its fsck intentionally exempts parents at those boundaries.

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
    let (mut content_type, mut content_length, mut content_encoding, mut git_protocol) =
        (String::new(), String::new(), String::new(), String::new());
    for header in request.headers() {
        match header.field.as_str().as_str().to_ascii_lowercase().as_str() {
            "content-type" => content_type = header.value.as_str().to_string(),
            "content-length" => content_length = header.value.as_str().to_string(),
            "content-encoding" => content_encoding = header.value.as_str().to_string(),
            "git-protocol" => git_protocol = header.value.as_str().to_string(),
            _ => {}
        }
    }

    let prefix = if path == "/git-receive-pack" && method == "POST" {
        // Git compresses upload-pack negotiation, not receive-pack requests.
        // Refuse an encoded push rather than bypassing boundary validation.
        if !content_encoding.is_empty() && content_encoding != "identity" {
            return request.respond(
                Response::from_string("encoded Git pushes are not supported")
                    .with_status_code(StatusCode(415)),
            );
        }
        match read_push_commands(request.as_reader()) {
            Ok(PushPrefix::Ready(prefix)) => prefix,
            Ok(PushPrefix::Rejected { message, sideband }) => {
                return respond_push_error(request, &message, sideband)
            }
            Err(error) => {
                return request.respond(
                    Response::from_string(error.to_string()).with_status_code(StatusCode(400)),
                )
            }
        }
    } else {
        Vec::new()
    };

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
        // Protocol v2 can fetch by hash even when the store advertises no refs.
        .env("HTTP_GIT_PROTOCOL", &git_protocol)
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
    let mut body = std::io::Cursor::new(prefix).chain(request.as_reader());
    let _ = std::io::copy(&mut body, &mut stdin);
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

/// What [`read_push_commands`] found: either the command packets to replay into
/// http-backend, or a refusal to hand back to the client.
///
/// A refusal is kept SEPARATE from an `Err` because the two reach the pusher by
/// different routes — see [`respond_push_error`]. `sideband` records whether the
/// client negotiated `side-band-64k`, which decides whether it can be told.
enum PushPrefix {
    Ready(Vec<u8>),
    Rejected { message: String, sideband: bool },
}

/// Read only receive-pack's command packets, leaving the pack itself streaming.
/// A shallow declaration can appear anywhere before the command flush, even
/// when the incomplete commit is not one of the refs being updated.
fn read_push_commands(mut input: impl Read) -> std::io::Result<PushPrefix> {
    let invalid = |message| std::io::Error::new(std::io::ErrorKind::InvalidData, message);
    let mut prefix = Vec::new();
    // SHALLOW LINES COME FIRST, capabilities second: the protocol is
    // `*shallow ( command-list | push-cert )`, and the capabilities ride on the
    // first COMMAND packet. So a refusal cannot be returned the moment a
    // `shallow` line is seen — at that point we do not yet know whether the
    // client can be told (side-band-64k), and guessing wrong means it is told
    // nothing. Read on to the command flush, which ends the command list and
    // never touches the pack, then refuse with the capabilities known.
    let mut sideband = false;
    let mut shallow = false;
    loop {
        let mut header = [0; 4];
        input.read_exact(&mut header)?;
        let length = std::str::from_utf8(&header)
            .ok()
            .and_then(|text| usize::from_str_radix(text, 16).ok())
            .ok_or_else(|| invalid("invalid Git command packet"))?;
        prefix.extend_from_slice(&header);
        if length == 0 {
            if shallow {
                return Ok(PushPrefix::Rejected {
                    message: "shallow pushes are not accepted; send complete history".into(),
                    sideband,
                });
            }
            return Ok(PushPrefix::Ready(prefix));
        }
        if length < 4 || prefix.len() + length - 4 > 16 * 1024 * 1024 {
            return Err(invalid("invalid or oversized Git command list"));
        }
        let start = prefix.len();
        prefix.resize(start + length - 4, 0);
        input.read_exact(&mut prefix[start..])?;
        if find(&prefix[start..], b"side-band-64k").is_some() {
            sideband = true;
        }
        if prefix[start..].starts_with(b"shallow ") {
            shallow = true;
        }
    }
}

/// Tell `git push` WHY it was refused, through the git protocol rather than an
/// HTTP status.
///
/// A non-2xx on `/git-receive-pack` is a transport failure to git's HTTP client:
/// it reports `error: RPC failed; HTTP 400` and DISCARDS the body. So the
/// sentence naming the only thing wrong, and the fix for it, reached nobody —
/// measured over an hour in which every `caosd up` publish failed, `git push`
/// said only `HTTP 400`, and the text became readable only by putting a proxy
/// on the wire. `GIT_CURL_VERBOSE=1` does not help; it prints headers, not
/// bodies.
///
/// Band 3 of the `side-band-64k` multiplexer is the error channel, and git
/// prints its payload verbatim behind `remote:`, exactly as it does a
/// pre-receive hook's rejection. A client that did not negotiate side-band gets
/// the old 400 — it cannot be told, but it can still report a status.
fn respond_push_error(request: Request, message: &str, sideband: bool) -> std::io::Result<()> {
    if !sideband {
        return request.respond(Response::from_string(message).with_status_code(StatusCode(400)));
    }
    let payload = format!("\x03{message}\n");
    let mut body = format!("{:04x}", payload.len() + 4).into_bytes();
    body.extend_from_slice(payload.as_bytes());
    body.extend_from_slice(b"0000");
    let content_type = Header::from_bytes(
        &b"Content-Type"[..],
        &b"application/x-git-receive-pack-result"[..],
    )
    .expect("static header is well formed");
    request.respond(
        Response::from_data(body)
            .with_status_code(StatusCode(200))
            .with_header(content_type),
    )
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
