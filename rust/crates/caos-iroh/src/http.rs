//! HTTP/1.1 over one iroh stream, blocking, for the caos client.
//!
//! The server is `tiny_http` and speaks ordinary HTTP; the only thing this
//! transport changes is what carries the bytes. So rather than invent a request
//! encoding, [`HttpClient`] writes a request onto a bi-stream and parses the
//! response off it — which also means the listener's HTTP handler is a plain
//! splice to the server's port, with nothing to keep in sync.
//!
//! BLOCKING ON THE OUTSIDE, async within: `caos` is a threaded blocking program
//! (`run_chat_turn` waits on `GET /run` in one thread while another polls
//! status), and it stays that way. One runtime and one connection live here for
//! the life of the process, and every request is a stream on that connection —
//! so a concurrent caller gets a stream, not a queue, and the object walk costs
//! a stream rather than a handshake.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncReadExt;
use tokio::runtime::Runtime;

use crate::{Client, RecvStream, SendStream, Service, Ticket};

/// A response, decoded far enough to hand back: everything after the header
/// block, de-chunked.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub body: Vec<u8>,
}

/// One runtime, and one connection per server, kept for the life of the process.
pub struct HttpClient {
    runtime: Runtime,
    /// Keyed by ticket URL. A process normally talks to one server, but the
    /// object API and a sub-run can name different ones, and a map costs
    /// nothing.
    ///
    /// `Arc` so that REPLACING an entry cannot cut off a request in flight: two
    /// threads can find the same dead connection and each dial a fresh one, and
    /// the loser's handle has to stay alive for as long as its own exchange is
    /// still using it.
    connections: Mutex<HashMap<String, Arc<Client>>>,
}

impl HttpClient {
    pub fn new() -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("starting a runtime for the caos:// transport: {e}"))?;
        Ok(Self {
            runtime,
            connections: Mutex::new(HashMap::new()),
        })
    }

    /// Send one request and read its response.
    ///
    /// `timeout` bounds the whole exchange. Most callers pass None — `GET /run`
    /// waits for the work — and the ones that pass a value would rather fail
    /// than wait (a reachability probe, a trace edge).
    pub fn request(
        &self,
        url: &str,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Option<&[u8]>,
        timeout: Option<std::time::Duration>,
    ) -> Result<Response, String> {
        let request = encode_request(method, path, headers, body);
        let exchange = async {
            // The handle is held for the whole exchange, not just the open: see
            // `connections`.
            let (client, send, recv) = self.open(url).await?;
            let response = exchange(send, recv, method, request).await;
            // The path can improve under a long-lived client — a connection
            // starts relayed and upgrades once holepunching succeeds — so the
            // trace reports it per request rather than once at connect.
            client.trace_path();
            drop(client);
            response
        };
        self.runtime.block_on(async {
            match timeout {
                Some(limit) => tokio::time::timeout(limit, exchange)
                    .await
                    .map_err(|_| format!("{method} {path}: timed out"))?,
                None => exchange.await,
            }
        })
    }

    /// A stream on this server's connection, dialing it the first time.
    ///
    /// RETRIED ONCE THROUGH A FRESH CONNECTION, because a cached one can be
    /// dead: the server restarted, the relay dropped us, the laptop slept. The
    /// retry is what keeps a long-lived client (the TUI) from needing a restart
    /// of its own, and it is safe to retry blind — no request byte has been
    /// written yet.
    async fn open(&self, url: &str) -> Result<(Arc<Client>, SendStream, RecvStream), String> {
        if let Some(client) = self.cached(url) {
            if let Ok((send, recv)) = client.open(Service::Http, None).await {
                return Ok((client, send, recv));
            }
        }
        let ticket = Ticket::parse(url)?;
        let client = Arc::new(Client::connect(&ticket).await?);
        let (send, recv) = client.open(Service::Http, None).await?;
        self.connections
            .lock()
            .expect("connection map")
            .insert(url.to_string(), client.clone());
        Ok((client, send, recv))
    }

    fn cached(&self, url: &str) -> Option<Arc<Client>> {
        self.connections
            .lock()
            .expect("connection map")
            .get(url)
            .cloned()
    }
}

/// Serialize a request.
///
/// `Connection: close` because the stream IS the connection: one request, one
/// stream, and the server's answer ends when it closes its half. `Host` is a
/// placeholder — HTTP/1.1 requires the field, and the far end of this stream is
/// one server's port, so there is nothing to choose between.
pub fn encode_request(
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Option<&[u8]>,
) -> Vec<u8> {
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: caos\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    let mut bytes = request.into_bytes();
    if let Some(body) = body {
        bytes.extend_from_slice(body);
    }
    bytes
}

/// Write one request onto a stream and read its response.
///
/// Public as the seam a test drives: given a stream pair from any listener, this
/// is the whole client, with no endpoint or runtime involved.
///
/// Finishing the send side is part of the request: `Connection: close` says this
/// exchange is the whole conversation, and the EOF is what makes the server act
/// on it rather than wait for a second request.
pub async fn exchange(
    mut send: SendStream,
    mut recv: RecvStream,
    method: &str,
    request: Vec<u8>,
) -> Result<Response, String> {
    send.write_all(&request)
        .await
        .map_err(|e| format!("writing the request: {e}"))?;
    send.finish().map_err(|e| format!("finishing: {e}"))?;
    read_response(&mut recv, method).await
}

/// Read one response, framed by its own headers.
///
/// THE FRAMING IS NOT OPTIONAL, and this is the one place it would be tempting to
/// skip: reading to EOF and parsing afterwards works for every small answer and
/// HANGS on the big ones. The server hands tiny_http `data_length = None` for
/// anything it streams — which is every object and every pack-sized body
/// (`server/src/git.rs` says why) — so those come back `Transfer-Encoding:
/// chunked` and the socket is NOT closed after the terminating chunk. Measured
/// symptom: a `caos-cli run` that printed nothing and never returned, while the
/// server's log showed its requests being answered one after another.
///
/// So read until the headers are complete, then until the BODY is complete by
/// whichever rule the headers give: the chunked terminator, `Content-Length`, or
/// end-of-stream if neither.
async fn read_response(recv: &mut RecvStream, method: &str) -> Result<Response, String> {
    let mut raw = Vec::new();
    let (head_end, body_start) = loop {
        if let Some(split) = find_header_end(&raw) {
            break split;
        }
        if !read_more(recv, &mut raw).await? {
            return Err(format!(
                "the response ended inside its headers after {} bytes",
                raw.len()
            ));
        }
    };
    let head = decode_head(&raw[..head_end])?;

    let mut body = raw[body_start..].to_vec();
    // A HEAD RESPONSE HAS NO BODY, whatever its headers say — and the caos server
    // says plenty: `has_object` and `server_holds` ask with HEAD, and tiny_http
    // answers with the `Content-Length` of the object it is NOT sending. Waiting
    // for those bytes gets end-of-stream instead, which read as
    // `the response ended after 0 of 96 body bytes` and made `server_holds`
    // answer "no" for objects the server holds — so every run re-pushed.
    // The same rule covers the bodiless statuses.
    let body = if method.eq_ignore_ascii_case("HEAD")
        || head.status == 204
        || head.status == 304
        || (100..200).contains(&head.status)
    {
        Vec::new()
    } else if head.chunked {
        loop {
            if let Some(decoded) = dechunk(&body)? {
                break decoded;
            }
            if !read_more(recv, &mut body).await? {
                return Err("the response ended inside a chunked body".to_string());
            }
        }
    } else if let Some(length) = head.content_length {
        while body.len() < length {
            if !read_more(recv, &mut body).await? {
                return Err(format!(
                    "the response ended after {} of {length} body bytes",
                    body.len()
                ));
            }
        }
        // A HEAD response carries the length of a body it does not send, so the
        // loop above is a no-op there and this keeps the caller from seeing
        // anything that followed.
        body.truncate(length);
        body
    } else {
        // Nothing frames it: the body is whatever arrives before the end.
        while read_more(recv, &mut body).await? {}
        body
    };
    Ok(Response {
        status: head.status,
        reason: head.reason,
        body,
    })
}

/// Append whatever arrives next. False at end of stream.
async fn read_more(recv: &mut RecvStream, into: &mut Vec<u8>) -> Result<bool, String> {
    let mut buf = [0u8; 64 * 1024];
    // The TOKIO `read`, spelled out: the stream carries an inherent one with a
    // different signature, and picking that up by accident would compile into
    // something subtly different.
    let read = AsyncReadExt::read(recv, &mut buf)
        .await
        .map_err(|e| format!("reading the response: {e}"))?;
    into.extend_from_slice(&buf[..read]);
    Ok(read > 0)
}

/// What a response's header block says about the rest of it.
struct Head {
    status: u16,
    reason: String,
    chunked: bool,
    content_length: Option<usize>,
}

fn decode_head(head: &[u8]) -> Result<Head, String> {
    let head = std::str::from_utf8(head).map_err(|_| "the response head is not UTF-8")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut fields = status_line.splitn(3, ' ');
    let version = fields.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(format!("not an HTTP response: {status_line:?}"));
    }
    let status: u16 = fields
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("no status code in {status_line:?}"))?;
    let reason = fields.next().unwrap_or("").to_string();

    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().ok();
        }
    }
    Ok(Head {
        status,
        reason,
        chunked,
        // Chunked wins, as HTTP requires: a body framed twice is a body framed
        // wrong, and honouring a length alongside it would cut it short.
        content_length: if chunked { None } else { content_length },
    })
}

/// Offsets of the end of the header block: where the head stops, and where the
/// body starts.
fn find_header_end(raw: &[u8]) -> Option<(usize, usize)> {
    raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| (at, at + 4))
        // A header block ended with bare LFs is malformed but cheap to accept,
        // and refusing it would turn a readable answer into a mystery.
        .or_else(|| {
            raw.windows(2)
                .position(|w| w == b"\n\n")
                .map(|at| (at, at + 2))
        })
}

/// Undo `Transfer-Encoding: chunked`.
///
/// THREE ANSWERS, not two, because the caller is reading as it goes: `Ok(Some)`
/// is a complete body, `Ok(None)` is "not yet, read more", and `Err` is framing
/// that can never become valid. Collapsing the first two — treating a short read
/// as an error — would fail every response larger than one read, and collapsing
/// the last two would spin forever on a malformed one.
fn dechunk(mut body: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut out = Vec::with_capacity(body.len());
    loop {
        let Some(line_end) = body.windows(2).position(|w| w == b"\r\n") else {
            return Ok(None);
        };
        let header = std::str::from_utf8(&body[..line_end])
            .map_err(|_| "a chunk header that is not UTF-8".to_string())?;
        // A chunk extension (`;name=value`) is legal and ignorable.
        let size = header.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| format!("a chunk size that is not hex: {size:?}"))?;
        body = &body[line_end + 2..];
        if size == 0 {
            // Trailers may follow; nothing here reads them, and the body is
            // complete either way.
            return Ok(Some(out));
        }
        // The chunk plus its trailing CRLF.
        if body.len() < size + 2 {
            return Ok(None);
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_carries_its_body_and_length() {
        let encoded = encode_request(
            "POST",
            "/object/",
            &[("x-caos", "1".to_string())],
            Some(b"blob 3\0abc"),
        );
        let text = String::from_utf8_lossy(&encoded);
        assert!(text.starts_with("POST /object/ HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("x-caos: 1\r\n"), "{text}");
        assert!(text.contains("Content-Length: 10\r\n"), "{text}");
        assert!(text.ends_with("\r\n\r\nblob 3\0abc"), "{text}");
    }

    #[test]
    fn a_head_gives_the_status_and_the_framing() {
        let head = decode_head(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nServer: x").expect("head");
        assert_eq!(head.status, 200);
        assert_eq!(head.reason, "OK");
        assert_eq!(head.content_length, Some(5));
        assert!(!head.chunked);
    }

    #[test]
    fn a_404_is_a_head_and_not_an_error() {
        // `has_object` asks with HEAD and reads 404 as "no", so the status has
        // to survive as data all the way to the caller.
        let head = decode_head(b"HTTP/1.1 404 Not Found").expect("head");
        assert_eq!(head.status, 404);
        assert_eq!(head.reason, "Not Found");
    }

    #[test]
    fn chunked_framing_beats_a_content_length_beside_it() {
        let head =
            decode_head(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 5")
                .expect("head");
        assert!(head.chunked);
        assert_eq!(head.content_length, None);
    }

    #[test]
    fn a_chunked_body_is_reassembled_without_its_framing() {
        // What the server sends for anything it streams, which is every large
        // object. The extension on the second chunk is legal and ignorable.
        let body = b"5\r\nhello\r\n1;ext=1\r\n \r\n5\r\nworld\r\n0\r\n\r\n";
        assert_eq!(dechunk(body).expect("valid"), Some(b"hello world".to_vec()));
    }

    #[test]
    fn an_incomplete_chunked_body_asks_for_more_rather_than_failing() {
        // The distinction the reader depends on: every response longer than one
        // read arrives here incomplete first.
        assert_eq!(dechunk(b"5\r\nhel").expect("not yet"), None);
        assert_eq!(dechunk(b"5\r\nhello\r\n").expect("not yet"), None);
        // A header without its line ending yet is also just "more to come".
        assert_eq!(dechunk(b"5").expect("not yet"), None);
    }

    #[test]
    fn a_chunk_size_that_is_not_hex_can_never_become_valid() {
        let error = dechunk(b"zz\r\nhello\r\n").expect_err("refused");
        assert!(error.contains("not hex"), "{error}");
    }
}
