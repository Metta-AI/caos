//! `iroh-vhost`: an HTTP front for `dumbpipe listen-tcp`.
//!
//! A browser asks for `http://<ticket chunks>.localhost:8080/path`. The ticket
//! is the dumbpipe ticket, lowercased base32 cut into DNS labels of at most 63
//! characters; this program reads it back out of the `Host` header, dials that
//! endpoint over iroh, and splices the TCP connection to a dumbpipe stream. The
//! path is never touched, because it is not where the ticket lives.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{presets, Connection, SendStream};
use iroh::{Endpoint, RelayMode};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

/// dumbpipe's ALPN, and the five bytes its listener waits for before it opens
/// the TCP side. A QUIC stream is invisible to the peer until the first write.
const ALPN: &[u8] = b"DUMBPIPEV0";
const HANDSHAKE: &[u8; 5] = b"hello";

const LABEL_MAX: usize = 63;
const NAME_MAX: usize = 253;
/// The request header that carries a ticket, for clients that can add headers
/// but cannot change the URL.
const TICKET_HEADER: &str = "Iroh-Ticket";
/// A ticket's text always begins with this, which is how a path segment that is
/// one is told from `assets`.
const TICKET_PREFIX: &str = "endpoint";
/// The longest request head we will buffer while looking for `Host`.
const HEAD_MAX: usize = 64 * 1024;

const USAGE: &str = "\
usage: iroh-vhost [--port N | --listen ADDR] [--suffix DOMAIN] [--host HOST[:PORT]]
       iroh-vhost url [--port N] [--suffix DOMAIN] TICKET

Serve on loopback, IPv4 and IPv6 (--port N, default 8080; --listen ADDR
pins one exact address instead). DOMAIN defaults to localhost. Browse to the URL that
`iroh-vhost url TICKET` prints; TICKET comes from `dumbpipe listen-tcp`.

A ticket is found in exactly one of three places, and a request with it in
more than one is refused:
  hostname  http://TICKET-LABELS.DOMAIN:8080/path   (what `url` prints)
  path      http://127.0.0.1:8080/TICKET/path       (a first segment starting
            with `endpoint`; stripped from the request before forwarding)
  header    Iroh-Ticket: TICKET                     (removed before forwarding)

--host sends the backend HOST[:PORT] as the Host header of every request (and
as the Origin of one that names the ticket's host), for backends that check it.";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = if args.first().map(String::as_str) == Some("url") {
        url_command(&args[1..])
    } else {
        serve_command(&args).await
    };
    if let Err(error) = result {
        eprintln!("iroh-vhost: {error}");
        std::process::exit(1);
    }
}

/// `--flag value` pairs, then positionals.
fn parse_flags(
    args: &[String],
    known: &[&str],
) -> Result<(HashMap<String, String>, Vec<String>), String> {
    let mut flags = HashMap::new();
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--help" || arg == "-h" {
            return Err(USAGE.to_string());
        }
        if let Some(name) = arg.strip_prefix("--") {
            if !known.contains(&name) {
                return Err(format!("unknown flag {arg}\n{USAGE}"));
            }
            let value = it.next().ok_or_else(|| format!("{arg} needs a value"))?;
            flags.insert(name.to_string(), value.clone());
        } else {
            rest.push(arg.clone());
        }
    }
    Ok((flags, rest))
}

fn url_command(args: &[String]) -> Result<(), String> {
    let (flags, rest) = parse_flags(args, &["port", "suffix"])?;
    let [ticket] = rest.as_slice() else {
        return Err(format!("url takes exactly one TICKET\n{USAGE}"));
    };
    let suffix = flags.get("suffix").map_or("localhost", String::as_str);
    let port = flags.get("port").map_or("8080", String::as_str);
    let _: EndpointTicket = ticket.parse().map_err(|e| format!("not a ticket: {e}"))?;
    let host = encode_host(ticket, suffix)?;
    println!("http://{host}:{port}/");
    Ok(())
}

/// Cut the ticket into labels and append the suffix.
fn encode_host(ticket: &str, suffix: &str) -> Result<String, String> {
    let ticket = ticket.to_ascii_lowercase();
    let labels: Vec<&str> = ticket
        .as_bytes()
        .chunks(LABEL_MAX)
        .map(|c| std::str::from_utf8(c).expect("ticket is ascii"))
        .collect();
    let host = format!("{}.{suffix}", labels.join("."));
    if host.len() > NAME_MAX {
        return Err(format!(
            "this ticket needs a {}-character hostname and the limit is {NAME_MAX}; \
             start `dumbpipe listen-tcp` so the ticket carries fewer addresses \
             (a relay and no direct addresses is enough)",
            host.len()
        ));
    }
    Ok(host)
}

/// The ticket a `Host` header value carries, as the string to parse.
fn decode_host(host: &str, suffix: &str) -> Result<String, String> {
    let name = host.rsplit_once(':').map_or(host, |(name, port)| {
        if port.bytes().all(|b| b.is_ascii_digit()) {
            name
        } else {
            host
        }
    });
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let body = name
        .strip_suffix(&format!(".{suffix}"))
        .ok_or_else(|| format!("Host {host:?} does not end in .{suffix}"))?;
    Ok(body.split('.').collect())
}

async fn serve_command(args: &[String]) -> Result<(), String> {
    let (flags, rest) = parse_flags(args, &["listen", "port", "suffix", "host"])?;
    if !rest.is_empty() {
        return Err(USAGE.to_string());
    }
    let suffix: Arc<str> = flags.get("suffix").map_or("localhost", String::as_str).into();
    let rewrite: Option<Arc<str>> = flags.get("host").map(|h| h.as_str().into());
    // `x.localhost` resolves to ::1 on some systems and to 127.0.0.1 on others,
    // and not every client retries the other family, so listen on both unless
    // told where. A machine without IPv6 just loses the second one.
    let explicit = flags.get("listen");
    if explicit.is_some() && flags.contains_key("port") {
        return Err("--listen names an address and port; use it or --port, not both".into());
    }
    let port: u16 = flags
        .get("port")
        .map_or(Ok(8080), |p| p.parse())
        .map_err(|e| format!("--port: {e}"))?;
    let addrs: Vec<String> = match explicit {
        Some(addr) => vec![addr.clone()],
        None => vec![format!("127.0.0.1:{port}"), format!("[::1]:{port}")],
    };
    let mut listeners = Vec::new();
    for addr in addrs {
        match TcpListener::bind(&addr).await {
            Ok(listener) => {
                eprintln!("iroh-vhost: listening on {addr}, hosts *.{suffix}");
                listeners.push(listener);
            }
            Err(error) if explicit.is_none() && addr.starts_with('[') => {
                eprintln!("iroh-vhost: not listening on {addr}: {error}");
            }
            Err(error) => return Err(format!("binding {addr}: {error}")),
        }
    }
    if let Some(host) = &rewrite {
        eprintln!("iroh-vhost: rewriting Host (and a matching Origin) to {host}");
    }
    let dialer = Arc::new(Dialer::default());
    let loops: Vec<_> = listeners
        .into_iter()
        .map(|listener| tokio::spawn(accept_loop(listener, dialer.clone(), suffix.clone(), rewrite.clone())))
        .collect();
    for task in loops {
        task.await.map_err(|e| format!("accept loop: {e}"))??;
    }
    Ok(())
}

async fn accept_loop(
    listener: TcpListener,
    dialer: Arc<Dialer>,
    suffix: Arc<str>,
    rewrite: Option<Arc<str>>,
) -> Result<(), String> {
    loop {
        let (stream, _) = listener.accept().await.map_err(|e| format!("accept: {e}"))?;
        let (dialer, suffix, rewrite) = (dialer.clone(), suffix.clone(), rewrite.clone());
        tokio::spawn(async move {
            if let Err(error) = handle(stream, &dialer, &suffix, rewrite.as_deref()).await {
                eprintln!("iroh-vhost: {error}");
            }
        });
    }
}

/// One iroh endpoint per ticket, kept for the next request, and a NEW
/// connection for every request.
///
/// The endpoint is the expensive part (it binds and reaches a relay). The
/// connection must not be shared: `dumbpipe listen-tcp` serves exactly one
/// stream per connection and then closes the connection, so a second stream on
/// it, or any stream racing the first, dies with "connection lost". That is
/// what `dumbpipe connect-tcp` does too: a connection per TCP connection.
#[derive(Default)]
struct Dialer {
    endpoints: Mutex<HashMap<String, Endpoint>>,
}

impl Dialer {
    async fn connection(&self, key: &str, ticket: &EndpointTicket) -> Result<Connection, String> {
        let mut addr = ticket.endpoint_addr().clone();
        // Debugging aid: keep only the endpoint id and relay, which is how a
        // listener on another network is reached, and which a listener on this
        // machine otherwise hides behind its direct addresses.
        if std::env::var_os("IROH_VHOST_RELAY_ONLY").is_some() {
            let mut relayed = iroh::EndpointAddr::from(addr.id);
            for relay in addr.relay_urls() {
                relayed = relayed.with_relay_url(relay.clone());
            }
            addr = relayed;
            eprintln!("iroh-vhost: IROH_VHOST_RELAY_ONLY: ignoring the ticket's direct addresses");
        }
        let endpoint = {
            let mut endpoints = self.endpoints.lock().await;
            match endpoints.get(key) {
                Some(endpoint) => endpoint.clone(),
                None => {
                    let endpoint = endpoint_for(&addr).await?;
                    endpoints.insert(key.to_string(), endpoint.clone());
                    endpoint
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(30), endpoint.connect(addr.clone(), ALPN))
            .await
            .map_err(|_| format!("connecting to {}: timed out", addr.id.fmt_short()))?
            .map_err(|e| format!("connecting to {}: {e}", addr.id.fmt_short()))
    }
}

/// An endpoint whose only relay is the ticket's own, so dialing never touches
/// a relay the ticket does not name.
async fn endpoint_for(addr: &iroh::EndpointAddr) -> Result<Endpoint, String> {
    let relays: Vec<String> = addr.relay_urls().map(|u| u.to_string()).collect();
    let builder = Endpoint::builder(presets::Minimal)
        .ca_tls_config(iroh_relay::tls::CaTlsConfig::system());
    let builder = if relays.is_empty() {
        builder.relay_mode(RelayMode::Disabled)
    } else {
        let map = iroh_relay::RelayMap::try_from_iter(relays.iter().map(String::as_str))
            .map_err(|e| format!("the ticket's relay is unusable: {e}"))?;
        builder.relay_mode(RelayMode::Custom(map))
    };
    builder.bind().await.map_err(|e| format!("binding an iroh endpoint: {e}"))
}

async fn handle(
    mut tcp: TcpStream,
    dialer: &Dialer,
    suffix: &str,
    new_host: Option<&str>,
) -> Result<(), String> {
    // Read until the head is complete: the Host header is in it, and every
    // byte read is replayed into the stream afterwards.
    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    let host = loop {
        let n = tcp.read(&mut chunk).await.map_err(|e| format!("reading request: {e}"))?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&chunk[..n]);
        if let Some(host) = find_host(&head) {
            break host;
        }
        if head.len() > HEAD_MAX {
            return refuse(tcp, "431 Request Header Fields Too Large", "no Host header").await;
        }
    };
    let (place, ticket_text) = match ticket_sources(&head, &host, suffix).as_slice() {
        [(place, text)] => (*place, text.clone()),
        [] => {
            let message = format!(
                "no ticket: put one in the hostname (<ticket>.{suffix}), as the first path \
                 segment (http://{host}/<ticket>/...), or in an {TICKET_HEADER} header"
            );
            return refuse(tcp, "400 Bad Request", &message).await;
        }
        several => {
            let places: Vec<&str> = several.iter().map(|(place, _)| *place).collect();
            let message = format!(
                "the ticket is given in more than one place ({}); use exactly one",
                places.join(" and ")
            );
            return refuse(tcp, "400 Bad Request", &message).await;
        }
    };
    let ticket: EndpointTicket = match ticket_text.parse() {
        Ok(ticket) => ticket,
        Err(error) => {
            let message = format!("the {place} does not hold a ticket: {error}");
            return refuse(tcp, "400 Bad Request", &message).await;
        }
    };
    let rewrite = Rewrite {
        host: new_host,
        strip_path: (place == "path").then_some(ticket_text.as_str()),
        drop_ticket_header: place == "header",
    };
    let connection = match dialer.connection(&ticket_text, &ticket).await {
        Ok(connection) => connection,
        Err(error) => return refuse(tcp, "502 Bad Gateway", &error).await,
    };
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| format!("opening a stream: {e}"))?;
    send.write_all(HANDSHAKE).await.map_err(|e| format!("handshake: {e}"))?;
    let (mut local_read, mut local_write) = tcp.into_split();
    // Each direction ends on its own: a client that has sent its request may
    // half-close while the response is still coming.
    let id = ticket.endpoint_addr().id.fmt_short();
    let trace = std::env::var_os("IROH_VHOST_TRACE").is_some();
    if trace {
        eprintln!("iroh-vhost: {id}: {place}, opened a stream");
    }
    let up = async {
        let result: io::Result<()> = if rewrite.is_active() {
            // Every request on the connection gets rewritten, so the bytes have
            // to be framed; otherwise they are just copied.
            forward_requests(local_read, &mut send, head, &rewrite).await
        } else {
            async {
                send.write_all(&head).await?;
                tokio::io::copy(&mut local_read, &mut send).await?;
                Ok(())
            }
            .await
        };
        if let Err(error) = result {
            eprintln!("iroh-vhost: {id}: sending the request: {error}");
        }
        let _ = send.finish();
    };
    let down = async {
        match tokio::io::copy(&mut recv, &mut local_write).await {
            // The stream ended without a byte: whatever answers for this
            // ticket accepted it and hung up, which is not the client's fault.
            Ok(0) => eprintln!(
                "iroh-vhost: {id}: the other end closed the stream without replying \
                 (its listener could not reach its target, or the target hung up)"
            ),
            Ok(n) if trace => eprintln!("iroh-vhost: {id}: {n} bytes back"),
            Ok(_) => {}
            // The Display of a read error says only "connection lost"; the
            // Debug form carries why (the peer's close reason, a timeout).
            Err(error) => eprintln!(
                "iroh-vhost: {id}: reading the reply: {error} ({error:?}); \
                 the connection is {}",
                match connection.close_reason() {
                    Some(reason) => format!("closed: {reason:?}"),
                    None => "still open".to_string(),
                }
            ),
        }
        let _ = local_write.shutdown().await;
    };
    tokio::join!(up, down);
    Ok(())
}

/// Every place a head names a ticket, as (place, ticket text). The caller
/// wants exactly one.
fn ticket_sources(head: &[u8], host: &str, suffix: &str) -> Vec<(&'static str, String)> {
    let mut found = Vec::new();
    if let Ok(text) = decode_host(host, suffix) {
        found.push(("hostname", text));
    }
    if let Some((text, _)) = request_target(head).and_then(split_ticket_segment) {
        if text.starts_with(TICKET_PREFIX) {
            found.push(("path", text));
        }
    }
    if let Some(value) = header_value(head, TICKET_HEADER) {
        found.push(("header", value.replace('.', "").to_ascii_lowercase()));
    }
    found
}

/// The request target (second word of the request line) of a head.
fn request_target(head: &[u8]) -> Option<&str> {
    let end = head.windows(2).position(|w| w == b"\r\n")?;
    let line = std::str::from_utf8(&head[..end]).ok()?;
    line.split(' ').nth(1)
}

/// Split `/<ticket>/rest?query` into the ticket (dots removed, lowercased, so a
/// chunked one works too) and the target the backend should see: `/rest?query`.
fn split_ticket_segment(target: &str) -> Option<(String, String)> {
    let after = target.strip_prefix('/')?;
    let end = after.find(['/', '?']).unwrap_or(after.len());
    let (segment, rest) = after.split_at(end);
    if segment.is_empty() {
        return None;
    }
    let remaining = match rest {
        "" => "/".to_string(),
        _ if rest.starts_with('?') => format!("/{rest}"),
        _ => rest.to_string(),
    };
    Some((segment.replace('.', "").to_ascii_lowercase(), remaining))
}

/// A request line with `/<ticket>` taken off its target, if it has that prefix.
fn strip_request_line(line: &[u8], ticket: &str) -> Option<String> {
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.splitn(3, ' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    let (found, remaining) = split_ticket_segment(target)?;
    (found == ticket).then(|| format!("{method} {remaining} {version}"))
}

/// The `Host` header value, once the head holds one. None while the head is
/// incomplete or has no such header.
fn find_host(head: &[u8]) -> Option<String> {
    header_value(head, "host")
}

/// The first header called `name` (any case) in a complete head.
fn header_value(head: &[u8], name: &str) -> Option<String> {
    let end = head.windows(4).position(|w| w == b"\r\n\r\n")?;
    let text = String::from_utf8_lossy(&head[..end]);
    text.split("\r\n").skip(1).find_map(|line| {
        let (field, value) = line.split_once(':')?;
        field.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_string())
    })
}

async fn refuse(mut tcp: TcpStream, status: &str, message: &str) -> Result<(), String> {
    let body = format!("iroh-vhost: {message}\n");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = tcp.write_all(response.as_bytes()).await;
    Err(format!("{status}: {message}"))
}

enum Framing {
    None,
    Length(usize),
    Chunked,
    /// A protocol switch (websocket): everything after the head is opaque.
    Upgrade,
}

fn is_ticket_header(line: &[u8]) -> bool {
    std::str::from_utf8(line)
        .ok()
        .and_then(|t| t.split_once(':'))
        .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case(TICKET_HEADER))
}

/// What to change in every request of a connection.
#[derive(Default)]
struct Rewrite<'a> {
    /// Replaces Host, and an Origin naming the Host the client sent.
    host: Option<&'a str>,
    /// A ticket whose `/<ticket>` prefix is taken off the request target.
    strip_path: Option<&'a str>,
    /// Remove the ticket header.
    drop_ticket_header: bool,
}

impl Rewrite<'_> {
    fn is_active(&self) -> bool {
        self.host.is_some() || self.strip_path.is_some() || self.drop_ticket_header
    }
}

/// `head` (through its blank line) rewritten as `rewrite` says, and how the
/// request's body is framed.
fn rewrite_head(head: &[u8], rewrite: &Rewrite) -> (Vec<u8>, Framing) {
    let (new_host, strip) = (rewrite.host, rewrite.strip_path);
    let original = find_host(head);
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut rest = &head[..head.len() - 4];
    while let Some(at) = rest.windows(2).position(|w| w == b"\r\n") {
        lines.push(&rest[..at]);
        rest = &rest[at + 2..];
    }
    lines.push(rest);
    let (mut length, mut chunked, mut upgrade) = (None, false, false);
    let mut out = Vec::with_capacity(head.len());
    for (i, line) in lines.iter().enumerate() {
        if i > 0 && rewrite.drop_ticket_header && is_ticket_header(line) {
            continue;
        }
        let field = std::str::from_utf8(line).ok().and_then(|t| t.split_once(':'));
        let replaced = match field {
            _ if i == 0 => strip.and_then(|ticket| strip_request_line(line, ticket)),
            Some((name, value)) => {
                let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
                match name.as_str() {
                    "host" => new_host.map(|host| format!("Host: {host}")),
                    "origin" => new_host.and_then(|host| {
                        original
                            .as_deref()
                            .filter(|h| value == format!("http://{h}"))
                            .map(|_| format!("Origin: http://{host}"))
                    }),
                    "content-length" => {
                        length = value.parse().ok();
                        None
                    }
                    "transfer-encoding" => {
                        chunked |= value.to_ascii_lowercase().contains("chunked");
                        None
                    }
                    "upgrade" => {
                        upgrade = true;
                        None
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        match replaced {
            Some(text) => out.extend_from_slice(text.as_bytes()),
            None => out.extend_from_slice(line),
        }
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    let framing = if upgrade {
        Framing::Upgrade
    } else if chunked {
        Framing::Chunked
    } else {
        match length {
            Some(n) if n > 0 => Framing::Length(n),
            _ => Framing::None,
        }
    };
    (out, framing)
}

/// The local half of a connection, with the bytes already read from it.
struct Buffered {
    read: OwnedReadHalf,
    buf: Vec<u8>,
}

impl Buffered {
    async fn fill(&mut self) -> io::Result<usize> {
        let mut chunk = [0u8; 8192];
        let n = self.read.read(&mut chunk).await?;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(n)
    }

    /// One line, CRLF included.
    async fn line(&mut self) -> io::Result<Vec<u8>> {
        loop {
            if let Some(at) = self.buf.windows(2).position(|w| w == b"\r\n") {
                return Ok(self.buf.drain(..at + 2).collect());
            }
            if self.fill().await? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
    }

    async fn copy_n(&mut self, mut n: usize, send: &mut SendStream) -> io::Result<()> {
        while n > 0 {
            if self.buf.is_empty() && self.fill().await? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let take = n.min(self.buf.len());
            send.write_all(&self.buf[..take]).await?;
            self.buf.drain(..take);
            n -= take;
        }
        Ok(())
    }
}

/// Forward each request on the connection with its Host rewritten, bodies
/// passed through unchanged.
async fn forward_requests(
    local: OwnedReadHalf,
    send: &mut SendStream,
    first: Vec<u8>,
    rewrite: &Rewrite<'_>,
) -> io::Result<()> {
    let mut local = Buffered { read: local, buf: first };
    loop {
        let end = loop {
            if let Some(at) = local.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            if local.fill().await? == 0 {
                let rest = std::mem::take(&mut local.buf);
                send.write_all(&rest).await?;
                return Ok(());
            }
        };
        let head: Vec<u8> = local.buf.drain(..end).collect();
        let (head, framing) = rewrite_head(&head, rewrite);
        send.write_all(&head).await?;
        match framing {
            Framing::None => {}
            Framing::Length(n) => local.copy_n(n, send).await?,
            Framing::Chunked => loop {
                let line = local.line().await?;
                send.write_all(&line).await?;
                let size = std::str::from_utf8(&line)
                    .ok()
                    .and_then(|t| t.trim().split(';').next())
                    .and_then(|t| usize::from_str_radix(t, 16).ok())
                    .ok_or_else(|| io::Error::other("bad chunk size"))?;
                if size == 0 {
                    // Trailers end at a blank line.
                    loop {
                        let trailer = local.line().await?;
                        send.write_all(&trailer).await?;
                        if trailer == b"\r\n" {
                            break;
                        }
                    }
                    break;
                }
                local.copy_n(size + 2, send).await?;
            },
            Framing::Upgrade => {
                let rest = std::mem::take(&mut local.buf);
                send.write_all(&rest).await?;
                tokio::io::copy(&mut local.read, send).await?;
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_round_trips_through_labels() {
        let ticket = "a".repeat(150);
        let host = encode_host(&ticket, "localhost").unwrap();
        assert!(host.split('.').all(|l| l.len() <= LABEL_MAX));
        assert_eq!(decode_host(&format!("{host}:8080"), "localhost").unwrap(), ticket);
    }

    #[test]
    fn overlong_ticket_is_refused() {
        assert!(encode_host(&"a".repeat(300), "localhost").is_err());
    }

    #[test]
    fn rewrites_host_and_frames_the_body() {
        let head = b"POST /x HTTP/1.1\r\nHost: a.b.localhost:8080\r\nOrigin: http://a.b.localhost:8080\r\nContent-Length: 5\r\n\r\n";
        let (out, framing) = rewrite_head(head, &Rewrite { host: Some("127.0.0.1:9119"), ..Default::default() });
        assert_eq!(
            out,
            b"POST /x HTTP/1.1\r\nHost: 127.0.0.1:9119\r\nOrigin: http://127.0.0.1:9119\r\nContent-Length: 5\r\n\r\n"
        );
        assert!(matches!(framing, Framing::Length(5)));
        let ws = b"GET /ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\n\r\n";
        assert!(matches!(rewrite_head(ws, &Rewrite { host: Some("z"), ..Default::default() }).1, Framing::Upgrade));
        let te = b"POST / HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(matches!(rewrite_head(te, &Rewrite { host: Some("z"), ..Default::default() }).1, Framing::Chunked));
    }

    #[test]
    fn splits_the_ticket_off_a_path() {
        let split = |t| split_ticket_segment(t);
        assert_eq!(split("/abc/x/y?q=1"), Some(("abc".into(), "/x/y?q=1".into())));
        assert_eq!(split("/ab.c"), Some(("abc".into(), "/".into())));
        assert_eq!(split("/ABC?q=1"), Some(("abc".into(), "/?q=1".into())));
        assert_eq!(split("/"), None);
        assert_eq!(split("*"), None);
    }

    #[test]
    fn strips_the_ticket_from_each_request_line() {
        let head = b"GET /abc/dash?x=1 HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n\r\n";
        let path = Rewrite { strip_path: Some("abc"), ..Default::default() };
        let (out, _) = rewrite_head(head, &path);
        assert_eq!(out, b"GET /dash?x=1 HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n\r\n");
        // No prefix, or another ticket's: forwarded as it came.
        let bare = b"GET /static/a.js HTTP/1.1\r\nHost: h\r\n\r\n";
        assert_eq!(rewrite_head(bare, &path).0, bare);
        let other = b"GET /xyz/a HTTP/1.1\r\nHost: h\r\n\r\n";
        assert_eq!(rewrite_head(other, &path).0, other);
        assert_eq!(request_target(head), Some("/abc/dash?x=1"));
    }

    #[test]
    fn counts_the_places_a_ticket_is_given() {
        let t = "endpointabc";
        let places = |request: String| {
            let head = request.as_bytes();
            let host = find_host(head).unwrap();
            ticket_sources(head, &host, "localhost")
                .iter()
                .map(|(place, _)| *place)
                .collect::<Vec<_>>()
        };
        let get = |target: &str, host: &str, extra: &str| {
            format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n")
        };
        assert_eq!(places(get("/x", &format!("{t}.localhost:1"), "")), ["hostname"]);
        assert_eq!(places(get(&format!("/{t}/x"), "127.0.0.1:1", "")), ["path"]);
        let header = format!("iroh-ticket: {t}\r\n");
        assert_eq!(places(get("/assets/a.js", "127.0.0.1:1", &header)), ["header"]);
        assert_eq!(places(get(&format!("/{t}/x"), "127.0.0.1:1", &header)), ["path", "header"]);
        assert!(places(get("/assets/a.js", "127.0.0.1:1", "")).is_empty());
    }

    #[test]
    fn drops_the_ticket_header() {
        let head = b"GET / HTTP/1.1\r\nHost: h\r\nIroh-Ticket: endpointabc\r\nAccept: */*\r\n\r\n";
        let drop = Rewrite { drop_ticket_header: true, ..Default::default() };
        assert_eq!(rewrite_head(head, &drop).0, b"GET / HTTP/1.1\r\nHost: h\r\nAccept: */*\r\n\r\n");
    }

    #[test]
    fn finds_host_case_insensitively() {
        let head = b"GET / HTTP/1.1\r\nhOsT:  x.localhost:1\r\nAccept: */*\r\n\r\n";
        assert_eq!(find_host(head).as_deref(), Some("x.localhost:1"));
        assert_eq!(find_host(b"GET / HTTP/1.1\r\nHost: x"), None);
    }
}
