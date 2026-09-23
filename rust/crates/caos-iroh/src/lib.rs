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
//! One iroh connection carries every kind of traffic, one BI-STREAM per request,
//! distinguished by a request line the opener writes first:
//!
//! ```text
//! caos1 <token> <service>[ <extra>]\n     ->   ok\n
//!                                          |   err <message>\n
//! ```
//!
//! The reply matters: a stream that fails to authenticate, or a git service the
//! listener cannot spawn, says so in a line the client can print, instead of
//! closing and leaving git to report "the remote end hung up unexpectedly".
//!
//! `<service>` is one of [`Service`]. For the git services `<extra>` carries a
//! `GIT_PROTOCOL` value, which the listener puts back in the environment of the
//! git it spawns — so the wire version is the client's to choose and it reaches
//! the far end. Proven by hand: with `version=2` the server answers a v2
//! capability advertisement (`000eversion 2`, `ls-refs=unborn`, …), without it a
//! v0 ref advertisement.
//!
//! **In practice git asks for v0 here**, and that is git's choice, not a gap in
//! this plumbing: it offers v2 to a remote helper only over
//! `stateless-connect`, which its own documentation calls experimental and for
//! internal use, and a helper advertising plain `connect` is spoken to in v0.
//! (A listener told `version=2` anyway is understood — git discovers the version
//! from the first pkt-line — so v2 is reachable without `stateless-connect`;
//! `git-remote-caos` says why it does not do that.)
//! Which costs little on this server — `uploadpack.hideRefs` already keeps
//! `refs/caos/req/` and `refs/caos/res/` out of fetch advertisements
//! (`server/src/main.rs`), so the advertisement a fetch reads is small. The
//! advertisement that does grow without bound is receive-pack's, where those
//! refs are deliberately visible, and v2 never applied to push at all.
//!
//! # Why a stream per request rather than a stream per connection
//!
//! Because the client is concurrent: `caos-cli`'s chat turn runs a long
//! `GET /run` on one thread while the main thread polls status, so a transport
//! that serialised onto a single stream would deadlock the poll behind the run.
//! Streams are also what makes the object walk affordable — `checkout` asks for
//! one object at a time by design, and each ask costs a stream, not a QUIC
//! handshake.
//!
//! # Authentication
//!
//! The ticket is a bearer capability: it carries an endpoint id AND a token, and
//! the listener refuses any stream whose token does not match. The token is what
//! makes it a capability rather than obscurity — an endpoint id is discoverable
//! by design (the listener publishes its addresses to DNS under it), so "knows
//! the id" cannot be the thing that authorizes driving a server that runs
//! containers and holds every secret. It also means a compromised ticket is
//! revoked by rotating the token, with the endpoint id — and so everyone's
//! remote URL — left alone.

use std::path::{Path, PathBuf};
use std::process::Stdio;
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

/// The ALPN both ends name. Versioned: a future incompatible request line gets
/// `caos/2` and an old listener refuses the connection outright, rather than
/// failing halfway through a stream.
pub const ALPN: &[u8] = b"caos/1";

/// URL scheme of a caos ticket remote. Also the name git resolves the remote
/// helper from: a `caos://` URL makes git exec `git-remote-caos` off PATH.
pub const URL_SCHEME: &str = "caos://";

/// First word of the request line, so a listener that somehow gets something
/// else on this ALPN fails on the first line with a diagnosis.
const REQUEST_MAGIC: &str = "caos1";

/// Length of the shared auth token, in bytes (hex-encoded in the ticket).
const TOKEN_BYTES: usize = 32;

/// Cap on a request or reply line. Both are short; anything longer is a client
/// talking a protocol we do not speak, and reading it unbounded would let one
/// stream chew memory.
const MAX_LINE: usize = 1024;

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

/// The endpoint builder both ends use, with the two settings a restricted
/// network needs.
///
/// ONE PLACE, because the client and the listener need these equally and a
/// setting applied to one of them is a failure that only shows up in the
/// placement nobody tests.
///
/// * **The OS trust store, not the copy of Mozilla's roots iroh compiles in.**
///   Where egress goes through a TLS-intercepting proxy — a Claude Code cloud
///   container is one — the relay presents that proxy's certificate, which
///   chains to a CA only the system knows about. A default build reaches NO
///   relay there ("Failed to connect to the home relay") on a host where curl
///   and git work perfectly, which reads as iroh being broken rather than as a
///   trust decision. This changes the RELAY HOP ONLY: payloads stay end-to-end
///   encrypted between endpoint keys, so the relay could not read them before
///   and cannot now.
/// * **The proxy the environment names** -- but ONLY for n0's default relays.
///   A relay named by `CAOS_IROH_RELAY` is dialled directly instead; see
///   [`endpoint_builder`] for why, and for how curl hid the difference.
///
/// Both come from `integrations/claude-code/cloud`, where they were first
/// carried as a patch against dumbpipe; they are the part of that patch worth
/// keeping.
/// `CAOS_IROH_RELAY` replaces n0's relays with one you run, and exists because
/// a Claude Code cloud container's SETUP phase cannot reach n0's at all.
///
/// Measured: from that phase all seven relays iroh ships answer `503` with an
/// Envoy `upstream connect error`, while `www.hetzner.com` answers 200 and four
/// of those relays ARE Hetzner-hosted. Bare IPs fail the same way, and so does
/// forcing http/1.1, so it is neither the name, nor the protocol, nor a route:
/// the gateway refuses those destinations. An `iroh-relay` on a host of one's
/// own, on port 80, answered 200 from the same phase in the same run.
///
/// An `http://` URL is legitimate here and needs no certificate: the relay
/// client reads TLS off the scheme (`use_tls()` is false for `http`) and
/// defaults such a URL to port 80. That matters because a non-standard port is
/// NOT carried by that egress at all -- the same relay on 3340 timed out from
/// both phases -- so 80 or 443 is the whole of the choice. The relay hop being
/// plaintext costs nothing that was not already given away: payloads stay
/// end-to-end encrypted between endpoint keys, as the note above says.
///
/// SAID OUT LOUD, both ways. A relay setting that silently does nothing is the
/// worst outcome here -- the symptom is "my change did not take", a full phase
/// away from the cause -- so a bad URL names itself rather than falling back to
/// n0 in silence. `caos-iroh serve` also prints the relays the ticket carries.
pub const RELAY_ENV: &str = "CAOS_IROH_RELAY";

/// The relay `CAOS_IROH_RELAY` names, parsed, or `None`.
///
/// Public because the TICKET has to carry it even when the endpoint has not
/// reached it. A ticket otherwise lists only relays actually CONNECTED to, so a
/// caosd started with no route out mints one with no relay at all -- usable
/// from its own LAN and nowhere else, with nothing saying so. Bringing a stack
/// up offline is an ordinary thing to do, and the configured relay is a
/// statement of intent rather than an observation: trust it, and let the
/// connection happen whenever the network does.
pub fn configured_relay() -> Option<iroh::RelayUrl> {
    let url = std::env::var_os(RELAY_ENV).map(|v| v.to_string_lossy().into_owned())?;
    if url.is_empty() {
        return None;
    }
    url.parse().ok()
}

pub fn endpoint_builder() -> iroh::endpoint::Builder {
    let builder =
        Endpoint::builder(presets::N0).ca_tls_config(iroh_relay::tls::CaTlsConfig::system());
    let Some(url) = std::env::var_os(RELAY_ENV)
        .map(|v| v.to_string_lossy().into_owned())
        .filter(|u| !u.is_empty())
    else {
        return builder.proxy_from_env();
    };
    match iroh_relay::RelayMap::try_from_iter([url.as_str()]) {
        Ok(map) => {
            // DIALLED DIRECTLY, and that is the whole reason this branch skips
            // `proxy_from_env`. iroh proxies EVERY relay dial once a proxy is
            // configured -- `dial_url` has no scheme test and no `no_proxy`
            // (neither crate mentions it) -- and it takes that proxy from
            // HTTP_PROXY, http_proxy, HTTPS_PROXY, https_proxy in turn. A cloud
            // session sets only `https_proxy`, so a plain `http://` relay was
            // reached as `CONNECT <host>:80` through it, and timed out.
            //
            // curl hid this: for an `http://` URL curl consults only
            // `http_proxy`, which is unset there, so it went DIRECT and
            // answered 200 while iroh could not connect at all. Verified:
            // `https_proxy=<black hole> curl http://<relay>/` still returns 200.
            //
            // A relay named here is one the operator chose FOR THIS NETWORK, so
            // direct is the right assumption; the proxy below exists for n0's
            // defaults, which such a network may not permit directly.
            eprintln!("caos-iroh: {RELAY_ENV}={url}: using it instead of n0's relays,");
            eprintln!(
                "caos-iroh:   dialled directly (no proxy, even if one is in the environment)"
            );
            builder.relay_mode(iroh::RelayMode::Custom(map))
        }
        Err(error) => {
            eprintln!("caos-iroh: {RELAY_ENV}={url} is not a relay URL ({error});");
            eprintln!(
                "caos-iroh:   falling back to n0's relays, which a cloud SETUP phase cannot reach"
            );
            builder.proxy_from_env()
        }
    }
}

/// Could `addr` be reached from another machine?
///
/// Used to decide which of the addresses an endpoint DISCOVERED about itself
/// survive when something outside also tells it where it can be reached
/// (`caos-iroh serve --advertise`). The discovered set is then a mixture: an
/// address on the container's own private network, which nothing outside can
/// route to and which changes every time the container is created; and, if the
/// relay's view of it got through, the public address its traffic comes from,
/// which is exactly what a client elsewhere needs. Keeping the second and
/// dropping the first is the whole of this predicate.
///
/// Private ranges are dropped rather than kept because the advertised addresses
/// replace them: whoever passed them in knows this machine's own addresses,
/// which the endpoint inside a container cannot see.
pub fn reachable_from_elsewhere(addr: &std::net::SocketAddr) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(ip) => {
            // 100.64.0.0/10 is carrier-grade NAT: a private range in all but
            // name, and `Ipv4Addr::is_shared` is still unstable.
            let cgnat = ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]);
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_documentation()
                || cgnat)
        }
        std::net::IpAddr::V6(ip) => {
            let unique_local = ip.segments()[0] & 0xfe00 == 0xfc00;
            // 2001:db8::/32, the v6 documentation range — `is_documentation` is
            // still unstable, and leaving it out would be an asymmetry with v4.
            let documentation = ip.segments()[0] == 0x2001 && ip.segments()[1] == 0x0db8;
            let link_local = ip.segments()[0] & 0xffc0 == 0xfe80;
            !(ip.is_loopback()
                || ip.is_unspecified()
                || unique_local
                || link_local
                || documentation)
        }
    }
}

/// What a stream asks the listener for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    /// Splice to the caos server's HTTP port: `/object`, `/run`, `/status`, …
    Http,
    /// `git upload-pack` on the server's object database (a fetch).
    GitUploadPack,
    /// `git receive-pack` on the server's object database (a push).
    GitReceivePack,
}

impl Service {
    pub fn as_str(self) -> &'static str {
        match self {
            Service::Http => "http",
            Service::GitUploadPack => "git-upload-pack",
            Service::GitReceivePack => "git-receive-pack",
        }
    }

    /// Parse a service name.
    ///
    /// BOTH SPELLINGS, because git uses both: a remote helper's `connect`
    /// command names the service `git-upload-pack`, while `git-remote-ext`'s
    /// `%s` expands to the bare `upload-pack`. Accepting one only works until
    /// the other form arrives.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name.strip_prefix("git-").unwrap_or(name) {
            "http" => Ok(Service::Http),
            "upload-pack" => Ok(Service::GitUploadPack),
            "receive-pack" => Ok(Service::GitReceivePack),
            _ => Err(format!("unknown service {name:?}")),
        }
    }

    /// The `git` subcommand serving this, or None for [`Service::Http`].
    pub fn git_subcommand(self) -> Option<&'static str> {
        match self {
            Service::Http => None,
            Service::GitUploadPack => Some("upload-pack"),
            Service::GitReceivePack => Some("receive-pack"),
        }
    }
}

/// Everything a client needs to reach one server: where the endpoint is, and the
/// token proving it may.
#[derive(Debug, Clone)]
pub struct Ticket {
    pub addr: EndpointAddr,
    pub token: String,
}

impl Ticket {
    /// The `caos://<endpoint ticket>.<token>` form that lives in a git remote's
    /// URL. `.` is a safe separator: an iroh ticket's own encoding is base32, so
    /// it contains no dot of its own.
    pub fn to_url(&self) -> String {
        let ticket = EndpointTicket::new(self.addr.clone());
        format!("{URL_SCHEME}{ticket}.{}", self.token)
    }

    pub fn parse(url: &str) -> Result<Self, String> {
        let body = url
            .strip_prefix(URL_SCHEME)
            .ok_or_else(|| format!("not a caos URL (expected {URL_SCHEME}<ticket>): {url:?}"))?;
        // From the RIGHT: the endpoint ticket is dot-free, the token is the last
        // field, and splitting the other way would break the moment a future
        // ticket encoding grows a separator.
        let (ticket, token) = body.rsplit_once('.').ok_or_else(|| {
            "caos URL has no token (expected caos://<ticket>.<token>)".to_string()
        })?;
        let ticket: EndpointTicket = ticket
            .parse()
            .map_err(|e| format!("invalid endpoint ticket in caos URL: {e}"))?;
        if hex_decode(token)?.len() != TOKEN_BYTES {
            return Err(format!(
                "caos URL token must be {TOKEN_BYTES} hex-encoded bytes"
            ));
        }
        Ok(Self {
            addr: ticket.endpoint_addr().clone(),
            token: token.to_string(),
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
    token: String,
}

impl Client {
    /// Dial the ticket's endpoint. The client's own key is EPHEMERAL: the
    /// listener authenticates the token, not the caller's identity, so there is
    /// no client key to store, lose, or have to enroll with a server.
    pub async fn connect(ticket: &Ticket) -> Result<Self, String> {
        let endpoint = endpoint_builder()
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
            addr = EndpointAddr::from(addr.id);
            eprintln!("caos-iroh: CAOS_IROH_RELAY_ONLY: ignoring the ticket's addresses");
        }
        let connection = endpoint
            .connect(addr, ALPN)
            .await
            .map_err(|e| format!("connecting to {}: {e}", ticket.addr.id.fmt_short()))?;
        let client = Self {
            endpoint,
            connection,
            token: ticket.token.clone(),
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

    /// Open one stream and ask for `service`. Returns once the listener has
    /// accepted it, so the caller can splice without a second handshake.
    ///
    /// `extra` is the client's `GIT_PROTOCOL` for a git service, None otherwise.
    pub async fn open(
        &self,
        service: Service,
        extra: Option<&str>,
    ) -> Result<(SendStream, RecvStream), String> {
        let (mut send, mut recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|e| format!("opening a stream: {e}"))?;
        let mut line = format!("{REQUEST_MAGIC} {} {}", self.token, service.as_str());
        if let Some(extra) = extra {
            // A newline here would forge a second request line; a space would
            // split one field into two. Neither can appear in a GIT_PROTOCOL
            // value, so refuse rather than sanitize.
            if extra.contains('\n') || extra.contains(' ') {
                return Err(format!("invalid request extra {extra:?}"));
            }
            line.push(' ');
            line.push_str(extra);
        }
        line.push('\n');
        send.write_all(line.as_bytes())
            .await
            .map_err(|e| format!("writing the request line: {e}"))?;
        let reply = read_line(&mut recv).await?;
        match reply.strip_prefix("err ") {
            Some(message) => Err(format!("{}: {message}", service.as_str())),
            None if reply == "ok" => Ok((send, recv)),
            None => Err(format!("unexpected reply from the server: {reply:?}")),
        }
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

/// Read one `\n`-terminated line, without buffering past it.
///
/// Byte at a time, deliberately: a `BufReader` would read ahead into bytes that
/// belong to the spliced payload and drop them when it is discarded. The lines
/// are under 200 bytes and the reads come out of the connection's own buffer.
pub async fn read_line(recv: &mut RecvStream) -> Result<String, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match recv.read_exact(&mut byte).await {
            Ok(()) => {}
            Err(e) if line.is_empty() => return Err(format!("reading a line: {e}")),
            Err(e) => return Err(format!("reading a line (after {line:?}): {e}")),
        }
        if byte[0] == b'\n' {
            return String::from_utf8(line).map_err(|_| "line is not UTF-8".to_string());
        }
        line.push(byte[0]);
        if line.len() > MAX_LINE {
            return Err(format!("line longer than {MAX_LINE} bytes"));
        }
    }
}

/// Copy in both directions until each side ends, then finish the stream.
///
/// Each direction completes INDEPENDENTLY, which is the whole requirement: git's
/// services half-close — `upload-pack` gets EOF on stdin while it is still
/// writing a pack — so a splice that tore down both halves when either finished
/// would truncate the pack. `try_join` also means an error in one direction
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

/// Read the hex-encoded auth token, creating one if the file is absent.
pub fn load_or_create_token(path: &Path) -> Result<String, String> {
    if let Some(text) = read_secret_file(path)? {
        if hex_decode(&text)?.len() != TOKEN_BYTES {
            return Err(format!(
                "{}: token must be {TOKEN_BYTES} hex-encoded bytes",
                path.display()
            ));
        }
        return Ok(text);
    }
    let token = hex_encode(&random_bytes()?);
    write_secret_file(path, &token)?;
    Ok(token)
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
/// `replace` says what an existing file means: `false` for a key or token, where
/// finding one is the normal case and overwriting it would invalidate every
/// ticket already handed out; `true` for the published ticket, which is rewritten
/// on every start.
///
/// The replace path UNLINKS FIRST and creates anew, rather than truncating: the
/// mode argument applies only when a file is created, so truncating an existing
/// world-readable file would leave it world-readable — and this file contains the
/// token, which is the capability itself.
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

/// Compare two tokens without leaking where they first differ.
pub fn tokens_match(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Parse a request line into its service and extra. The magic and token are
/// checked here too, so a listener has one place to reject a stream.
pub fn parse_request(line: &str, token: &str) -> Result<(Service, Option<String>), String> {
    let mut fields = line.split(' ');
    let magic = fields.next().unwrap_or_default();
    if magic != REQUEST_MAGIC {
        return Err(format!("unexpected protocol {magic:?}"));
    }
    let presented = fields.next().ok_or("request line has no token")?;
    if !tokens_match(presented, token) {
        return Err("unauthorized".to_string());
    }
    let service = Service::parse(fields.next().ok_or("request line has no service")?)?;
    let extra = fields.next().map(|e| e.to_string());
    Ok((service, extra))
}

/// The serving half: what one accepted connection's streams are answered with.
///
/// Held in an `Arc` and shared by every connection, because it is configuration
/// and nothing else — the state a request touches is the object database git
/// opens and the socket HTTP is spliced to.
pub struct Server {
    /// `host:port` of the caos server's own HTTP listener, for [`Service::Http`].
    pub to: String,
    /// The object database git services are served from. None refuses them,
    /// which is a real placement: a listener in front of a server that has no
    /// repo to offer.
    pub git_dir: Option<PathBuf>,
    /// The token every stream must present.
    pub token: String,
}

impl Server {
    /// Answer one connection's streams until the peer closes it.
    ///
    /// MANY STREAMS PER CONNECTION, each spawned: a client opens one per request
    /// and has more than one in flight (a long `GET /run` alongside a status
    /// poll), so answering them in sequence here would serialise the client.
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

    /// Read one request line, authenticate it, and answer it.
    pub async fn answer(&self, send: SendStream, mut recv: RecvStream) -> Result<(), String> {
        let line = read_line(&mut recv).await?;
        let (service, extra) = match parse_request(&line, &self.token) {
            Ok(request) => request,
            Err(error) => return refuse(send, &error).await,
        };
        match (service.git_subcommand(), &self.git_dir) {
            (None, _) => self.splice_to_http(send, recv).await,
            (Some(subcommand), Some(git_dir)) => {
                serve_git(send, recv, subcommand, git_dir, extra.as_deref()).await
            }
            (Some(_), None) => {
                let error = format!("this server serves no git ({})", service.as_str());
                refuse(send, &error).await
            }
        }
    }

    async fn splice_to_http(&self, mut send: SendStream, recv: RecvStream) -> Result<(), String> {
        let stream = match TcpStream::connect(&self.to).await {
            Ok(stream) => stream,
            Err(error) => {
                return refuse(send, &format!("connecting to {}: {error}", self.to)).await
            }
        };
        send.write_all(b"ok\n")
            .await
            .map_err(|e| format!("accepting the stream: {e}"))?;
        let (read, write) = stream.into_split();
        splice(send, recv, read, write).await
    }
}

/// Tell the client why, rather than closing the stream and leaving git to report
/// that the remote hung up. Not an error for the listener: a refused stream is
/// an answered stream.
async fn refuse(mut send: SendStream, error: &str) -> Result<(), String> {
    eprintln!("caos-iroh: refused: {error}");
    let _ = send.write_all(format!("err {error}\n").as_bytes()).await;
    let _ = send.finish();
    Ok(())
}

/// Serve a fetch or a push by spawning git on the object database.
///
/// Straight to `git upload-pack` / `git receive-pack`, with no `http-backend` in
/// front, and that is not a way past the protections the caos server installs on
/// this repo. Every one of them — `gc.auto=0`, `receive.autogc=false`,
/// `maintenance.geometric-repack.enabled=false`, `uploadpack.allowFilter`, the
/// hidden refs — is GIT CONFIG on the repo itself, so it binds whoever opens it.
/// The one setting that is http-only is `http.receivepack`, which a direct
/// `receive-pack` never consults.
async fn serve_git(
    mut send: SendStream,
    recv: RecvStream,
    subcommand: &str,
    git_dir: &Path,
    git_protocol: Option<&str>,
) -> Result<(), String> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg(subcommand)
        .arg(git_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Inherited: git's diagnostics belong in this member's log, next to the
        // line naming the stream they came from.
        .stderr(Stdio::inherit());
    // The wire version the client asked for, put back where git looks for it.
    // REMOVED rather than left alone when the client asked for nothing: this
    // process inherits the environment of whatever started the listener, and an
    // ambient `GIT_PROTOCOL` there would make the service answer a version the
    // client is not speaking.
    match git_protocol {
        Some(value) => command.env("GIT_PROTOCOL", value),
        None => command.env_remove("GIT_PROTOCOL"),
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return refuse(send, &format!("spawning git {subcommand}: {error}")).await,
    };
    send.write_all(b"ok\n")
        .await
        .map_err(|e| format!("accepting the stream: {e}"))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let result = splice(send, recv, stdout, stdin).await;
    // Reap it either way; its exit status is git's own diagnosis of a failed
    // fetch or push and belongs in the log.
    match child.wait().await {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("caos-iroh: git {subcommand} exited {status}"),
        Err(error) => eprintln!("caos-iroh: waiting for git {subcommand}: {error}"),
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;

    fn a_ticket() -> Ticket {
        let key = SecretKey::from_bytes(&[7u8; 32]);
        Ticket {
            addr: EndpointAddr::new(key.public()),
            token: hex_encode(&[9u8; TOKEN_BYTES]),
        }
    }

    #[test]
    fn ticket_round_trips_through_a_url() {
        let ticket = a_ticket();
        let url = ticket.to_url();
        assert!(is_caos_url(&url));
        let parsed = Ticket::parse(&url).expect("parses");
        assert_eq!(parsed.addr.id, ticket.addr.id);
        assert_eq!(parsed.token, ticket.token);
    }

    #[test]
    fn a_url_without_a_token_is_refused() {
        let ticket = EndpointTicket::new(a_ticket().addr);
        let error = Ticket::parse(&format!("{URL_SCHEME}{ticket}")).expect_err("no token");
        assert!(error.contains("no token"), "{error}");
    }

    #[test]
    fn only_addresses_another_machine_could_use_survive_advertising() {
        let keep = |addr: &str| reachable_from_elsewhere(&addr.parse().expect("an address"));
        // What a container discovers about itself, and what a relay observes
        // about it: the first is unreachable and changes per container, the
        // second is what a client on another network needs.
        assert!(!keep("10.89.0.25:11204"), "container network");
        assert!(!keep("192.168.1.10:11204"), "home LAN");
        assert!(!keep("172.17.0.2:11204"), "docker bridge");
        assert!(!keep("127.0.0.1:11204"), "loopback");
        assert!(!keep("169.254.7.7:11204"), "link-local");
        assert!(!keep("100.100.1.1:11204"), "carrier-grade NAT");
        assert!(keep("69.181.90.11:11204"), "a public address");
        assert!(!keep("[2001:db8::1]:11204"), "documentation v6");
        assert!(!keep("[fe80::1]:11204"), "v6 link-local");
        assert!(!keep("[fc00::1]:11204"), "v6 unique-local");
        assert!(keep("[2606:4700::1111]:11204"), "a public v6 address");
    }

    #[test]
    fn hex_round_trips() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes);
        assert!(hex_decode("0g").is_err());
        assert!(hex_decode("abc").is_err());
    }

    #[test]
    fn a_request_line_carries_the_service_and_git_protocol() {
        let token = hex_encode(&[1u8; TOKEN_BYTES]);
        let line = format!("{REQUEST_MAGIC} {token} git-upload-pack version=2");
        let (service, extra) = parse_request(&line, &token).expect("parses");
        assert_eq!(service, Service::GitUploadPack);
        assert_eq!(extra.as_deref(), Some("version=2"));
    }

    #[test]
    fn a_bad_token_is_unauthorized_whatever_the_service() {
        let token = hex_encode(&[1u8; TOKEN_BYTES]);
        let line = format!("{REQUEST_MAGIC} {} http", hex_encode(&[2u8; TOKEN_BYTES]));
        let error = parse_request(&line, &token).expect_err("refused");
        assert_eq!(error, "unauthorized");
    }
}
