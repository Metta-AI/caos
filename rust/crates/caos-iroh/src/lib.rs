//! The `caos://` transport: one caos server, reached by TICKET instead of by
//! address.
//!
//! A client's whole configuration is the remote URL in its own `.git/config`:
//!
//! ```text
//! [remote "caos"]
//!     url = caos://<ticket>
//! ```
//!
//! which is what makes this per-worktree. There is no local port to allocate and
//! no daemon to keep alive, so two checkouts pointed at two different servers
//! cannot collide, and neither can two people on one machine.
//!
//! # Shape
//!
//! **This is dumbpipe's protocol, and nothing else.** The listener answers the
//! ALPN `DUMBPIPEV0`; an opener writes the five bytes `hello` on a fresh
//! bi-stream and from then on the stream IS a TCP connection to the one port the
//! listener was told to forward to (the caos server's HTTP port). There is no
//! request line, no service to name and no reply. So a ticket from `caos-iroh
//! serve` is a dumbpipe ticket: stock `dumbpipe connect-tcp`, `iroh-vhost` and
//! anything else that speaks dumbpipe reach the server with it, and everything
//! the server offers is a URL path — `/object`, `/run`, `/eval`, `/mcp`, and git
//! smart HTTP at `/info/refs`, `/git-upload-pack` and `/git-receive-pack`.
//!
//! One thing is added to what dumbpipe does, and it is invisible to a stock
//! client: the listener keeps a connection open and answers MANY streams on it.
//! Stock `dumbpipe listen-tcp` serves one stream and closes, so every request
//! there pays a QUIC handshake. A client of ours holds one connection and opens a
//! stream per request — `caos-cli`'s chat turn runs a long `GET /run` on one
//! thread while another polls status, and the object walk asks for one object
//! at a time, so a stream must be cheap and independent.
//!
//! # Authentication
//!
//! There is none beyond the ticket. The endpoint id is a public key, and
//! knowing it (with a relay or an address) is the whole credential, exactly as
//! with `dumbpipe`. An earlier version added a shared token to every stream; it
//! was dropped, deliberately, because it made the protocol ours alone and a
//! ticket is already something to keep private. Treat one as you would a
//! password to the server: it runs containers and holds every secret.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub mod http;

use iroh::endpoint::{presets, Connection, Incoming};
/// The stream halves callers splice; re-exported so they need no iroh dependency
/// of their own.
pub use iroh::endpoint::{RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, SecretKey};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// The ALPN dumbpipe names, which is what makes a ticket from here a dumbpipe
/// ticket and a dumbpipe client a client of ours.
pub const ALPN: &[u8] = b"DUMBPIPEV0";

/// The five bytes dumbpipe's opener writes before anything else. A stream is
/// invisible to the peer's `accept_bi` until it carries data, so this is what
/// makes it appear at all.
pub const HELLO: &[u8; 5] = b"hello";

/// URL scheme of a caos ticket remote. Also the name git resolves the remote
/// helper from: a `caos://` URL makes git exec `git-remote-caos` off PATH.
pub const URL_SCHEME: &str = "caos://";

/// How long to wait for the endpoint to learn its own addresses before minting a
/// ticket. Exceeding it is not fatal — an id-only ticket still resolves through
/// DNS — so this only bounds how long `serve` waits to print one.
pub const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a client will wait to notify the server that it is done. See
/// [`Client::close`] for why this is capped at all.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(25);

/// The UDP port a listener binds unless told otherwise, and the reason a ticket
/// can carry addresses at all.
///
/// A TICKET IS ONLY DURABLE IF THE ADDRESSES IN IT ARE, and the volatile part is
/// the port: a listener that takes whatever the kernel hands it prints a
/// different ticket after every restart. Pinning it makes the addresses — and so
/// the ticket — stable, which is what lets them be published, which is what gets
/// clients a DIRECT path.
///
/// And that is worth a great deal. Measured against a server on the same
/// machine: with an address in the ticket the connection goes DIRECT in 0.10s —
/// even when that address is unreachable, since the real one is then learned over
/// the relay. With none it stays relayed (probed for 45s, one path, no upgrade),
/// paying a relay round trip per request: a cached `caos-cli run` (32 sequential
/// requests) took 4.6 s instead of ~100 ms.
///
/// 11204 is iroh's own default port, so a firewall rule written for one tool
/// fits the other. `--port 0` asks for an ephemeral one, and takes the
/// unstable-ticket behaviour back.
pub const DEFAULT_PORT: u16 = 11204;

/// The endpoint builder both ends use, and the settings a restricted network
/// needs.
///
/// ONE PLACE, because the client and the listener need these equally and a
/// setting applied to one of them is a failure that only shows up in the
/// placement nobody tests.
///
/// **N0'S RELAYS AND N0'S DNS ARE NEVER USED.** `presets::Minimal` sets the
/// crypto provider and nothing else, where `presets::N0` would add the default
/// relay map, a pkarr publisher and a resolver against `iroh.link`. A Claude Code
/// cloud container's SETUP phase cannot reach any of them: all seven relays iroh
/// ships answer `503` there with an Envoy `upstream connect error`, under h2 and
/// http/1.1 alike and by bare IP, while `www.hetzner.com` answers 200 from the
/// same phase and four of those relays ARE Hetzner-hosted. The relay is named
/// explicitly instead, by whoever runs the server, and nothing falls back.
///
/// Discovery is not needed either: a `caos://` ticket carries the endpoint id,
/// the relay and the direct addresses, which is everything a dial takes.
///
/// **THE OS TRUST STORE, not the copy of Mozilla's roots iroh compiles in.**
/// Where egress goes through a TLS-intercepting proxy — a Claude Code cloud
/// container is one — an `https://` relay presents that proxy's certificate,
/// which chains to a CA only the system knows about, and a default build reaches
/// NO relay there ("Failed to connect to the home relay") on a host where curl
/// and git work perfectly. This changes the RELAY HOP ONLY: payloads stay
/// end-to-end encrypted between endpoint keys.
///
/// **NO PROXY, EITHER END.** iroh routes EVERY relay dial through a configured
/// proxy — `dial_url` has no scheme test and no `no_proxy`, and it takes
/// HTTP_PROXY, http_proxy, HTTPS_PROXY, https_proxy in turn — so a plain
/// `http://` relay is dialled as `CONNECT <host>:80` and times out wherever a
/// proxy variable is set. curl hides this, consulting only `http_proxy` for an
/// `http://` URL, so a relay curl reaches directly is one iroh cannot reach at
/// all. Nothing here needs a proxy: a relay named for this network is either
/// directly reachable or unusable.
fn base_endpoint_builder() -> iroh::endpoint::Builder {
    Endpoint::builder(presets::Minimal).ca_tls_config(iroh_relay::tls::CaTlsConfig::system())
}

/// The relay map for one URL, or why that URL cannot be one.
///
/// An `http://` URL is legitimate and needs no certificate: the relay client
/// reads TLS off the scheme (`use_tls()` is false for `http`) and defaults such a
/// URL to port 80. PORT 80 OR 443 AND NOTHING ELSE, because a cloud container's
/// egress carries no other port — the same relay on 3340 times out from both the
/// setup and the session phase. A plaintext relay hop gives nothing away:
/// payloads stay end-to-end encrypted between endpoint keys.
pub fn relay_map(relay: &iroh::RelayUrl) -> Result<iroh_relay::RelayMap, String> {
    iroh_relay::RelayMap::try_from_iter([relay.as_str()])
        .map_err(|error| format!("{relay} is not usable as a relay: {error}"))
}

/// The LISTENER's builder: the relay it will be reached through, REQUIRED and
/// with no default.
///
/// A fallback here is worse than an error, because of the shape of the failure it
/// produces: a ticket carrying an unreachable relay keeps its endpoint id, so it is
/// indistinguishable from the ticket already in someone's
/// environment, and every session against it fails a whole phase away with
/// `connecting to <id>: timed out`. Which relay to use is a question only the
/// operator can answer.
pub fn endpoint_builder(relay: &iroh::RelayUrl) -> Result<iroh::endpoint::Builder, String> {
    Ok(base_endpoint_builder().relay_mode(iroh::RelayMode::Custom(relay_map(relay)?)))
}

/// The CLIENT's builder: the relay comes from the TICKET, which is the only thing
/// a client is given.
///
/// The ticket's relay becomes the client's OWN relay map, not just the peer's
/// address. Left at a default, the client connects to an n0 relay as its own home
/// relay before dialling anything — latency where that is permitted, failure
/// where it is not.
///
/// A ticket with no relay is LAN-only and says so: a stack brought up with no
/// route out still serves the machine it runs on.
pub fn client_endpoint_builder(ticket: &Ticket) -> iroh::endpoint::Builder {
    let relays: Vec<String> = ticket
        .addr
        .relay_urls()
        .map(|url| url.to_string())
        .collect();
    let builder = base_endpoint_builder();
    if relays.is_empty() {
        eprintln!("caos-iroh: this ticket carries no relay; only its direct addresses can work");
        return builder.relay_mode(iroh::RelayMode::Disabled);
    }
    match iroh_relay::RelayMap::try_from_iter(relays.iter().map(String::as_str)) {
        Ok(map) => builder.relay_mode(iroh::RelayMode::Custom(map)),
        Err(error) => {
            eprintln!("caos-iroh: this ticket's relays are unusable ({error});");
            eprintln!("caos-iroh:   only its direct addresses can work");
            builder.relay_mode(iroh::RelayMode::Disabled)
        }
    }
}

/// Everything a client needs to reach one server: where the endpoint is.
#[derive(Debug, Clone)]
pub struct Ticket {
    pub addr: EndpointAddr,
}

impl Ticket {
    /// The `caos://<endpoint ticket>` form that lives in a git remote's URL.
    /// The part after the scheme is exactly the ticket `dumbpipe` prints.
    pub fn to_url(&self) -> String {
        let ticket = EndpointTicket::new(self.addr.clone());
        format!("{URL_SCHEME}{ticket}")
    }

    pub fn parse(url: &str) -> Result<Self, String> {
        let body = url
            .strip_prefix(URL_SCHEME)
            .ok_or_else(|| format!("not a caos URL (expected {URL_SCHEME}<ticket>): {url:?}"))?;
        // A `.<token>` suffix is IGNORED rather than refused. Tickets minted
        // before the token was dropped carry one (`caos://<ticket>.<hex>`), they
        // sit in git remotes and cloud environments, and an iroh ticket's own
        // encoding is base32, which has no dot.
        let ticket = body.split_once('.').map_or(body, |(ticket, _)| ticket);
        let ticket: EndpointTicket = ticket
            .parse()
            .map_err(|e| format!("invalid endpoint ticket in caos URL: {e}"))?;
        Ok(Self {
            addr: ticket.endpoint_addr().clone(),
        })
    }
}

/// True if `url` names this transport. Cheap enough to call on every server URL,
/// which is how the client decides between minreq and iroh.
pub fn is_caos_url(url: &str) -> bool {
    url.starts_with(URL_SCHEME)
}

/// A connected client: one QUIC connection, and a stream per request.
pub struct Client {
    /// Held so the connection stays alive — dropping the endpoint closes it.
    endpoint: Endpoint,
    connection: Connection,
}

impl Client {
    /// Dial the ticket's endpoint. The client's own key is EPHEMERAL: the
    /// listener does not authenticate the caller's identity, so there is
    /// no client key to store, lose, or have to enroll with a server.
    pub async fn connect(ticket: &Ticket) -> Result<Self, String> {
        let endpoint = client_endpoint_builder(ticket)
            .bind()
            .await
            .map_err(|e| format!("binding an iroh endpoint: {e}"))?;
        Self::connect_on(endpoint, ticket).await
    }

    /// Dial on an endpoint the caller already has.
    ///
    /// Two callers want this: a test, which binds a relay-less endpoint so it
    /// talks to a listener over loopback and needs no network at all; and (in
    /// time) a long-lived client that keeps one endpoint across several servers.
    pub async fn connect_on(endpoint: Endpoint, ticket: &Ticket) -> Result<Self, String> {
        // `CAOS_IROH_RELAY_ONLY` drops the ticket's IP hints, leaving the relay as
        // the only way to the server. It exists because relay-only is the CLOUD's
        // permanent condition and nothing on a developer's machine reproduces it:
        // a ticket names private addresses, a container on the same host reaches
        // them directly at 49 MB/s, and a container across the internet cannot
        // reach them at all. A bug that only appears on the relayed path -- a
        // large push dying partway -- is otherwise unreproducible outside a cloud
        // session, which is the worst place to debug one.
        let mut addr = ticket.addr.clone();
        if std::env::var_os("CAOS_IROH_RELAY_ONLY").is_some() {
            // THE RELAY IS KEPT. Dropping it too leaves an address with nothing
            // in it but an endpoint id, and this client has no discovery to fill
            // that in -- n0's DNS is not used -- so the dial fails with "No
            // addressing information available" rather than going relayed.
            let mut relayed = EndpointAddr::from(addr.id);
            for relay in addr.relay_urls() {
                relayed = relayed.with_relay_url(relay.clone());
            }
            addr = relayed;
            eprintln!("caos-iroh: CAOS_IROH_RELAY_ONLY: ignoring the ticket's addresses");
        }
        let connection = endpoint
            .connect(addr, ALPN)
            .await
            .map_err(|e| format!("connecting to {}: {e}", ticket.addr.id.fmt_short()))?;
        let client = Self {
            endpoint,
            connection,
        };
        client.trace_path();
        Ok(client)
    }

    /// Under `CAOS_IROH_TRACE`, say which path this connection is using.
    ///
    /// THE FIRST QUESTION about a slow ticket server, and invisible otherwise: a
    /// relayed path costs a round trip to the relay and back on EVERY request,
    /// and every request in a run is sequential. Measured on a server in a
    /// container on the same machine, relayed: 135 ms per request, ~4.6 s for a
    /// cached run. The same run direct is a tenth of that.
    pub fn trace_path(&self) {
        if std::env::var_os("CAOS_IROH_TRACE").is_none() {
            return;
        }
        eprintln!("caos-iroh: {}", self.path_description());
    }

    /// How this connection currently reaches the server, in one line: which path
    /// is carrying traffic, its round-trip time, and how many are open.
    pub fn path_description(&self) -> String {
        let paths = self.connection.paths();
        match paths.iter().find(|path| path.is_selected()) {
            Some(path) => format!(
                "path {} {} rtt {:?} ({} open)",
                if path.is_relay() { "RELAY" } else { "direct" },
                path.remote_addr(),
                path.rtt(),
                paths.len()
            ),
            None => format!("no path selected yet ({} open)", paths.len()),
        }
    }

    /// Open one stream to the server's HTTP port. Returns at once: the stream is
    /// a TCP connection from here on, and the first thing on it is the caller's
    /// request.
    ///
    /// The `hello` is dumbpipe's handshake and the only thing this adds. There
    /// is no reply to wait for, so a server that cannot be reached shows up as a
    /// stream that ends or a response that never comes — the listener answers a
    /// backend it cannot dial with an HTTP 502 saying so.
    pub async fn open(&self) -> Result<(SendStream, RecvStream), String> {
        let (mut send, recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|e| format!("opening a stream: {e}"))?;
        send.write_all(HELLO)
            .await
            .map_err(|e| format!("writing the handshake: {e}"))?;
        Ok((send, recv))
    }

    /// Tell the peer we are done, so it frees the connection now instead of
    /// waiting out an idle timeout.
    ///
    /// BOUNDED, because a full graceful close is not worth what it costs a
    /// short-lived client. `Endpoint::close` waits long enough for the
    /// CONNECTION_CLOSE frame to be very likely delivered, and it does not
    /// return early when that happens promptly — measured, it is a flat ~1s:
    /// `git ls-remote` over this transport took 1.02s with it, 0.023s without,
    /// and 0.155s with the cap below. 40x, on an operation whose whole job is
    /// one ref advertisement, and the helper runs once per git command. The
    /// frame itself is queued by `Connection::close` and goes out in the first
    /// transmit, so the wait buys reliability of NOTIFICATION, not of data —
    /// and the cap is long enough for that transmit in practice (the server
    /// logs `closed by peer` for these connections rather than timing them
    /// out).
    ///
    /// Safe to call as soon as [`splice`] returns, and only then: closing
    /// discards data the peer has received but not yet delivered to its
    /// application, so a push must not close before it has read the server's
    /// report — which is exactly what the end of the splice means.
    pub async fn close(self) {
        // Again at the end, because the interesting question is whether the path
        // IMPROVED: a connection starts relayed and upgrades to direct once
        // holepunching succeeds, so one reading at connect time cannot tell a
        // slow upgrade from none at all.
        self.trace_path();
        self.connection.close(0u32.into(), b"done");
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, self.endpoint.close()).await;
    }
}

/// Copy in both directions until each side ends, then finish the stream.
///
/// Each direction completes INDEPENDENTLY, which is the whole requirement: a
/// request that ends its body half-closes while the response is still
/// coming — so a splice that tore down both halves when either finished
/// would truncate it. `try_join` also means an error in one direction
/// drops the other, which is the right answer for a broken connection.
pub async fn splice(
    mut send: SendStream,
    mut recv: RecvStream,
    mut local_read: impl AsyncRead + Unpin,
    mut local_write: impl AsyncWrite + Unpin,
) -> Result<(), String> {
    let trace = std::env::var_os("CAOS_IROH_TRACE").is_some();
    // OWNERSHIP MOVES INTO EACH DIRECTION, so each local half is DROPPED the
    // moment that direction ends. For a child process that drop is the whole
    // point: `shutdown()` on a `tokio::process::ChildStdin` flushes and returns,
    // it does NOT close the pipe, so a git service on the far side would keep
    // waiting for input that can never come.
    //
    // Which is exactly how a failed push hung. `git pack-objects` can fail
    // locally — "unable to read <oid>", the ordinary case where a client pushes a
    // request whose base it cannot read (AGENTS.md, `tests/push-closure`) — and
    // git then closes the helper's stdin without sending a pack. Over HTTP the
    // server ends `receive-pack`'s stdin when the request body ends (`git.rs`
    // drops it for this reason) and git reports the failure in milliseconds; here
    // `receive-pack` sat in `read` forever while git waited for its report, and
    // `caos-cli` never reached the object-by-object fallback that makes such a
    // push work at all.
    let up = async move {
        let moved = tokio::io::copy(&mut local_read, &mut send)
            .await
            .map_err(|e| format!("copying to the stream: {e}"))?;
        if trace {
            eprintln!("caos-iroh: up done after {moved} bytes");
        }
        send.finish().map_err(|e| format!("finishing: {e}"))?;
        drop(local_read);
        Ok(())
    };
    let down = async move {
        let moved = tokio::io::copy(&mut recv, &mut local_write)
            .await
            .map_err(|e| format!("copying from the stream: {e}"))?;
        if trace {
            eprintln!("caos-iroh: down done after {moved} bytes");
        }
        local_write
            .shutdown()
            .await
            .map_err(|e| format!("closing the local side: {e}"))?;
        // THE CLOSE. See above: dropping it is what ends the child's input.
        drop(local_write);
        Ok(())
    };
    tokio::try_join!(up, down).map(|_| ())
}

/// Read a hex-encoded secret key, creating one if the file is absent.
///
/// The file is the endpoint's IDENTITY, so persisting it is what keeps a ticket
/// valid across restarts: regenerate the key and every remote URL a client holds
/// is stale. Created 0600 — it is a private key.
pub fn load_or_create_secret(path: &Path) -> Result<SecretKey, String> {
    let existing = read_secret_file(path)?;
    if let Some(text) = existing {
        return text
            .parse()
            .map_err(|e| format!("{}: invalid secret key: {e}", path.display()));
    }
    let key = SecretKey::from_bytes(&random_bytes()?);
    write_secret_file(path, &hex_encode(&key.to_bytes()))?;
    Ok(key)
}

fn read_secret_file(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("reading {}: {e}", path.display())),
    }
}

fn write_secret_file(path: &Path, contents: &str) -> Result<(), String> {
    write_private(path, contents, false)
}

/// Write `contents` to a file only its owner can read.
///
/// `replace` says what an existing file means: `false` for a key, where
/// finding one is the normal case and overwriting it would invalidate every
/// ticket already handed out; `true` for the published ticket, which is rewritten
/// on every start.
///
/// The replace path UNLINKS FIRST and creates anew, rather than truncating: the
/// mode argument applies only when a file is created, so truncating an existing
/// world-readable file would leave it world-readable — and a ticket file is the
/// capability itself.
pub fn write_private(path: &Path, contents: &str, replace: bool) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    if replace {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("replacing {}: {e}", path.display())),
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("creating {}: {e}", path.display()))?;
    write!(file, "{contents}").map_err(|e| format!("writing {}: {e}", path.display()))
}

/// 32 bytes from the kernel. Read rather than generated in-process so this crate
/// needs no RNG dependency of its own.
fn random_bytes() -> Result<[u8; 32], String> {
    use std::io::Read;

    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("opening /dev/urandom: {e}"))?
        .read_exact(&mut bytes)
        .map_err(|e| format!("reading /dev/urandom: {e}"))?;
    Ok(bytes)
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).expect("nibble"));
        out.push(char::from_digit((byte & 0xf) as u32, 16).expect("nibble"));
    }
    out
}

pub fn hex_decode(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("hex string has an odd length".to_string());
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| format!("not hex: {:?}", pair[0] as char))?;
        let lo = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| format!("not hex: {:?}", pair[1] as char))?;
        out.push((hi * 16 + lo) as u8);
    }
    Ok(out)
}

/// The serving half: what one accepted connection's streams are answered with.
///
/// Held in an `Arc` and shared by every connection, because it is configuration
/// and nothing else.
pub struct Server {
    /// `host:port` of the caos server's own HTTP listener. Every stream is a TCP
    /// connection to it.
    pub to: String,
}

impl Server {
    /// Answer one connection's streams until the peer closes it.
    ///
    /// MANY STREAMS PER CONNECTION, each spawned: a client opens one per request
    /// and has more than one in flight (a long `GET /run` alongside a status
    /// poll), so answering them in sequence here would serialise the client.
    /// This is the one thing beyond dumbpipe's own listener, which serves a
    /// single stream and closes; a stock client sends one and loses nothing.
    pub async fn accept(self: Arc<Self>, incoming: Incoming) -> Result<(), String> {
        let connection = incoming
            .accept()
            .map_err(|e| format!("accepting: {e}"))?
            .await
            .map_err(|e| format!("completing the handshake: {e}"))?;
        let remote = connection.remote_id().fmt_short();
        eprintln!("caos-iroh: connected {remote}");
        loop {
            let (send, recv) = match connection.accept_bi().await {
                Ok(pair) => pair,
                // Includes the ordinary end: the peer closed the connection.
                Err(error) => {
                    eprintln!("caos-iroh: {remote} done: {error}");
                    return Ok(());
                }
            };
            let server = self.clone();
            tokio::spawn(async move {
                if let Err(error) = server.answer(send, recv).await {
                    eprintln!("caos-iroh: stream: {error}");
                }
            });
        }
    }

    /// Read the handshake and splice the stream to the backend.
    pub async fn answer(&self, mut send: SendStream, mut recv: RecvStream) -> Result<(), String> {
        let mut hello = [0u8; 5];
        recv.read_exact(&mut hello)
            .await
            .map_err(|e| format!("reading the handshake: {e}"))?;
        if &hello != HELLO {
            return Err(format!("unexpected handshake {hello:?}, not `hello`"));
        }
        let stream = match TcpStream::connect(&self.to).await {
            Ok(stream) => stream,
            Err(error) => {
                // The stream carries HTTP, so say so in HTTP: a client that
                // reads a response sees the reason instead of a hangup.
                let message = format!("caos-iroh: connecting to {}: {error}\n", self.to);
                eprintln!("{message}");
                let response = format!(
                    "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{message}",
                    message.len()
                );
                let _ = send.write_all(response.as_bytes()).await;
                let _ = send.finish();
                return Ok(());
            }
        };
        let (read, write) = stream.into_split();
        splice(send, recv, read, write).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_ticket() -> Ticket {
        let key = SecretKey::from_bytes(&[7u8; 32]);
        Ticket {
            addr: EndpointAddr::new(key.public()),
        }
    }

    #[test]
    fn ticket_round_trips_through_a_url() {
        let ticket = a_ticket();
        let url = ticket.to_url();
        assert!(is_caos_url(&url));
        let parsed = Ticket::parse(&url).expect("parses");
        assert_eq!(parsed.addr.id, ticket.addr.id);
    }

    /// The part after `caos://` is the ticket dumbpipe prints, so one pasted from
    /// either tool is the other's.
    #[test]
    fn the_url_is_the_endpoint_ticket_and_nothing_else() {
        let ticket = a_ticket();
        let bare = EndpointTicket::new(ticket.addr.clone()).to_string();
        assert_eq!(ticket.to_url(), format!("{URL_SCHEME}{bare}"));
    }

    /// Tickets minted while a token was part of the protocol are still in git
    /// remotes and cloud environments.
    #[test]
    fn a_url_that_still_carries_a_token_still_parses() {
        let ticket = a_ticket();
        let old = format!("{}.{}", ticket.to_url(), "ab".repeat(32));
        assert_eq!(Ticket::parse(&old).expect("parses").addr.id, ticket.addr.id);
    }

    #[test]
    fn hex_round_trips() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes);
        assert!(hex_decode("0g").is_err());
        assert!(hex_decode("abc").is_err());
    }

    /// dumbpipe's ALPN and handshake, byte for byte: they are what make a stock
    /// client work.
    #[test]
    fn the_protocol_is_dumbpipes() {
        assert_eq!(ALPN, b"DUMBPIPEV0");
        assert_eq!(HELLO, b"hello");
    }
}
