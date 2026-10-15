//! The server end of the `caos://` transport, plus a stdio pipe for driving it
//! by hand.
//!
//! ```text
//! caos-iroh serve --state <dir> --to <host:port> --relay <url> [--git-dir <dir>] [--port <n>]
//! caos-iroh ticket --state <dir>
//! caos-iroh pipe <service> <url>
//! ```
//!
//! `serve` runs beside a caos server — as a member of the stack group, so it
//! shares the server's lifetime and its log directory. Every stream it accepts
//! is one request:
//!
//! * `http` is spliced to `--to`, the server's own HTTP port, so `/object`,
//!   `/run` and `/status` work unchanged.
//! * `git-upload-pack` / `git-receive-pack` spawn that git service on
//!   `--git-dir` directly.
//!
//! SPAWNING GIT DIRECTLY IS NOT A SHORTCUT PAST THE SERVER'S PROTECTIONS, and
//! it's worth knowing why, because it looks like one. Everything the server
//! asserts to keep this object database safe — `gc.auto=0`,
//! `receive.autogc=false`, `maintenance.geometric-repack.enabled=false`,
//! `uploadpack.allowFilter`, the hidden-ref configuration — is written as GIT
//! CONFIG on the repo (`server/src/main.rs`), so it binds any process that opens
//! it, not merely the `http-backend` the server happens to exec. The one
//! genuinely http-only setting is `http.receivepack`, which a direct
//! `receive-pack` never consults.
//!
//! `--state` holds three files, all 0600: `key`, the endpoint's identity, and
//! `token`, the shared secret — both created on first use — plus the `ticket`
//! this serving publishes. Persisting the first two is what makes a ticket
//! durable: a new key or token would invalidate every remote URL already in
//! someone's `.git/config`.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use caos_iroh::{
    load_or_create_secret, load_or_create_token, splice, Relay, Server, Service, Ticket, ALPN,
    ONLINE_TIMEOUT, RELAY_ALPN,
};
use iroh::SecretKey;

const USAGE: &str = "\
usage:
  caos-iroh serve --state <dir> --to <host:port> --relay <url>
                  [--git-dir <dir>] [--port <n>]
                  [--advertise <host:port> ...]
  caos-iroh relay --state <dir> --to <host:port> --relay <url>
                  [--port <n>] [--advertise <host:port> ...]
  caos-iroh ticket --state <dir>
  caos-iroh pipe <service> <url>
  caos-iroh relay-pipe <url>
  caos-iroh probe <url> [seconds]

  serve   answer caos:// streams: HTTP ones spliced to --to, git ones served
          from --git-dir, on UDP --port (default 11204; a FIXED port is what
          keeps the ticket stable). Writes the ticket to <dir>/ticket and
          prints it.
          --relay is the relay clients reach this endpoint through, and is
          REQUIRED: n0's are never used, and a default would mint a ticket
          that looks right and reaches nothing. `http://<host>/` is fine
          (port 80 or 443 and nothing else).
          --advertise REPLACES the addresses it finds for itself with ones it
          cannot discover — a published container port, say. Repeatable; it
          never removes the relays, so a client always has a way in.
  relay   a SEPARATE, simpler endpoint: every stream opened against it is
          spliced straight to --to, with no request line and no service to
          name -- not `serve`'s protocol, a different ALPN entirely. For a
          backend with its own real auth (a login page, say), where the
          caos/git multiplexing protocol would be a second gate nobody asked
          for. Same flags, own ticket, own state dir -- point --state
          somewhere other than a `serve` on the same machine uses.
  ticket  print the ticket, for handing to a client. Reads <dir>/ticket (works
          for either `serve`'s or `relay`'s state dir).
  probe   report how a client reaches that server, once a second: a RELAY path
          costs a round trip to the relay on every request.
  pipe    splice stdin/stdout to one `serve` stream (<service> is http,
          git-upload-pack or git-receive-pack). What `ext::` remotes use.
  relay-pipe  splice stdin/stdout to one `relay` stream. For testing a relay
          ticket by hand.
";

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("caos-iroh: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("serve") => serve(Options::parse(args)?).await,
        Some("relay") => relay(Options::parse(args)?).await,
        Some("ticket") => ticket(Options::parse(args)?),
        Some("probe") => {
            let url = args.next().ok_or_else(|| USAGE.to_string())?;
            let seconds = match args.next() {
                Some(raw) => raw
                    .parse()
                    .map_err(|error| format!("probe seconds {raw:?}: {error}"))?,
                None => 30,
            };
            probe(&url, seconds).await
        }
        Some("pipe") => {
            let service = args.next().ok_or_else(|| USAGE.to_string())?;
            let url = args.next().ok_or_else(|| USAGE.to_string())?;
            pipe(&service, &url).await
        }
        Some("relay-pipe") => {
            let url = args.next().ok_or_else(|| USAGE.to_string())?;
            relay_pipe(&url).await
        }
        _ => Err(USAGE.to_string()),
    }
}

/// Flags shared by `serve` and `ticket`.
#[derive(Default)]
struct Options {
    /// Addresses to put in the ticket INSTEAD of the ones the endpoint discovers,
    /// for a listener whose own view of itself is not what clients can reach.
    advertise: Vec<std::net::SocketAddr>,
    /// UDP port for the endpoint. None means `caos_iroh::DEFAULT_PORT`.
    port: Option<u16>,
    state: Option<PathBuf>,
    to: Option<String>,
    git_dir: Option<PathBuf>,
    /// The relay clients reach this endpoint through. Required for `serve`; see
    /// `caos_iroh::endpoint_builder` for why it has no default.
    relay: Option<iroh::RelayUrl>,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = Options::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or_else(|| format!("{arg} needs a value\n{USAGE}"))
            };
            match arg.as_str() {
                "--state" => options.state = Some(value()?.into()),
                "--to" => options.to = Some(value()?),
                "--git-dir" => options.git_dir = Some(value()?.into()),
                "--relay" => {
                    let raw = value()?;
                    let url = raw
                        .parse()
                        .map_err(|error| format!("--relay {raw:?}: {error}\n{USAGE}"))?;
                    options.relay = Some(url);
                }
                "--port" => {
                    let raw = value()?;
                    let port = raw
                        .parse()
                        .map_err(|error| format!("--port {raw:?}: {error}\n{USAGE}"))?;
                    options.port = Some(port);
                }
                "--advertise" => {
                    let raw = value()?;
                    let addr = raw
                        .parse()
                        .map_err(|error| format!("--advertise {raw:?}: {error}\n{USAGE}"))?;
                    options.advertise.push(addr);
                }
                other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
            }
        }
        Ok(options)
    }

    fn state(&self) -> Result<&Path, String> {
        self.state
            .as_deref()
            .ok_or_else(|| format!("--state is required\n{USAGE}"))
    }

    /// The relay to be reached through. No default, and no fallback to n0's:
    /// see `caos_iroh::endpoint_builder`.
    fn relay(&self) -> Result<&iroh::RelayUrl, String> {
        self.relay.as_ref().ok_or_else(|| {
            format!(
                "--relay is required: it is the relay clients reach this endpoint\n  \
                 through, and n0's are never used -- a cloud container's setup phase\n  \
                 answers 503 for all seven of them. Run `iroh-relay --dev` on a host\n  \
                 of your own, on port 80 or 443, and name it here.\n{USAGE}"
            )
        })
    }

    /// The persisted identity and shared token, created on first use.
    fn identity(&self) -> Result<(SecretKey, String), String> {
        let state = self.state()?;
        let key = load_or_create_secret(&state.join("key"))?;
        let token = load_or_create_token(&state.join("token"))?;
        Ok((key, token))
    }

    /// The UDP port to bind. `0` asks the kernel for a free one and gives up a
    /// stable ticket in exchange — see `caos_iroh::DEFAULT_PORT`.
    fn port(&self) -> u16 {
        self.port.unwrap_or(caos_iroh::DEFAULT_PORT)
    }

    /// Where `serve` publishes the ticket and `ticket` reads it. ONE PATH rather
    /// than a flag, so the two subcommands cannot disagree about it.
    fn ticket_file(&self) -> Result<PathBuf, String> {
        Ok(self.state()?.join("ticket"))
    }
}

/// Bind an iroh endpoint for a listener: the given identity and ALPN, on a
/// FIXED UDP port (both families) when one was named, over the relay
/// `options.relay()` names. Shared by `serve` and `relay` -- the ALPN is the
/// only thing that differs between what they bind.
async fn bind_endpoint(
    options: &Options,
    key: SecretKey,
    alpn: &[u8],
) -> Result<(iroh::Endpoint, iroh::RelayUrl), String> {
    let relay = options.relay()?.clone();
    let mut builder = caos_iroh::endpoint_builder(&relay)?
        .secret_key(key)
        .alpns(vec![alpn.to_vec()]);
    // A FIXED PORT, so the addresses this endpoint advertises are the same ones
    // after a restart — which is what lets the ticket carry them (see
    // DEFAULT_PORT). Both families, or the v6 half would still drift.
    let port = options.port();
    if port != 0 {
        builder = builder
            .bind_addr(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
            .map_err(|e| format!("binding UDP port {port}: {e}"))?
            .bind_addr(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0))
            .map_err(|e| format!("binding UDP port {port} (v6): {e}"))?;
    }
    let endpoint = builder
        .bind()
        .await
        .map_err(|e| format!("binding an iroh endpoint: {e}"))?;
    // Wait for our own addresses before minting a ticket, so the one printed
    // carries a relay a client can reach immediately rather than relying on
    // DNS discovery. Not fatal if it times out: the endpoint id in the ticket
    // resolves on its own, it just may take a first-connection detour.
    if tokio::time::timeout(ONLINE_TIMEOUT, endpoint.online())
        .await
        .is_err()
    {
        eprintln!("caos-iroh: warning: no relay yet; the ticket may take longer to resolve");
    }
    Ok((endpoint, relay))
}

/// Compute the ticket address per `--advertise`, write it to `<state>/ticket`,
/// and return the ticket plus its direct/relay strings for logging. Shared by
/// `serve` and `relay` — see the long comment this was lifted from in
/// `serve`'s git history for why nothing here infers an address.
fn mint_ticket(
    options: &Options,
    endpoint: &iroh::Endpoint,
    relay: iroh::RelayUrl,
    token: String,
) -> Result<(Ticket, Vec<String>, Vec<String>), String> {
    // THE RELAY GOES IN WHETHER OR NOT IT HAS BEEN REACHED — see `serve`'s own
    // comment for why `addr()` alone is not enough here.
    let discovered = if endpoint.addr().relay_urls().any(|u| *u == relay) {
        endpoint.addr()
    } else {
        endpoint.addr().with_relay_url(relay)
    };
    // NO `--advertise`, NO ADDRESSES: an endpoint's view of itself in a
    // container reaches it from nowhere and changes on every recreation.
    let addr = if options.advertise.is_empty() {
        let mut addr = iroh::EndpointAddr::new(discovered.id);
        for relay in discovered.relay_urls() {
            addr = addr.with_relay_url(relay.clone());
        }
        addr
    } else {
        let mut addr = iroh::EndpointAddr::new(discovered.id);
        for relay in discovered.relay_urls() {
            addr = addr.with_relay_url(relay.clone());
        }
        for extra in &options.advertise {
            addr = addr.with_ip_addr(*extra);
        }
        addr
    };
    let direct: Vec<String> = addr.ip_addrs().map(|a| a.to_string()).collect();
    let relays: Vec<String> = addr.relay_urls().map(|u| u.to_string()).collect();
    let ticket = Ticket { addr, token };
    // WRITTEN BEFORE THE FIRST ACCEPT, so whatever brought this process up can
    // wait on the file and then hand the ticket out. 0600 like the token it
    // contains.
    let path = options.ticket_file()?;
    caos_iroh::write_private(&path, &format!("{}\n", ticket.to_url()), true)?;
    Ok((ticket, direct, relays))
}

async fn serve(options: Options) -> Result<(), String> {
    let (key, token) = options.identity()?;
    let server = Arc::new(Server {
        to: options
            .to
            .clone()
            .ok_or_else(|| format!("--to is required\n{USAGE}"))?,
        git_dir: options.git_dir.clone(),
        token: token.clone(),
    });
    let (endpoint, relay) = bind_endpoint(&options, key, ALPN).await?;
    let (ticket, direct, relays) = mint_ticket(&options, &endpoint, relay, token)?;
    // REPORTED FROM THE TICKET, not from the endpoint: with `--advertise` the
    // two differ, and the ticket's set is the one clients act on. An empty one
    // means every client is relayed wherever it sits — worth saying out loud,
    // because the only other symptom is that everything is slow.
    eprintln!("caos-iroh: serving {} as {}", server.to, endpoint.addr().id);
    if direct.is_empty() {
        eprintln!("caos-iroh: ticket has no direct addresses — every client will be relayed");
    } else {
        eprintln!("caos-iroh: ticket addresses: {}", direct.join(" "));
    }
    eprintln!("caos-iroh: relays: {}", relays.join(" "));
    match &server.git_dir {
        Some(dir) => eprintln!("caos-iroh: git services on {}", dir.display()),
        None => eprintln!("caos-iroh: no --git-dir: git services will be refused"),
    }
    let url = ticket.to_url();
    eprintln!("caos-iroh: ticket: {url}");
    eprintln!("caos-iroh: a client uses it as: git remote add caos {url}");

    while let Some(incoming) = endpoint.accept().await {
        let server = server.clone();
        tokio::spawn(async move {
            if let Err(error) = server.accept(incoming).await {
                // A connection failing is ordinary — a client went away, a
                // stale ticket, a probe. Log it and keep serving.
                eprintln!("caos-iroh: connection: {error}");
            }
        });
    }
    // An ERROR, not a quiet end: the loop only ends when the endpoint is gone,
    // and a ticket that no longer answers is exactly the failure worth being
    // loud about. As a stack member this also takes the group down with a
    // reason, rather than exiting 0 and leaving `serve` to guess.
    Err("the iroh endpoint stopped accepting connections".to_string())
}

/// A relay endpoint: see [`caos_iroh::Relay`] for what it does and why it is
/// deliberately not `serve`'s protocol.
async fn relay(options: Options) -> Result<(), String> {
    let (key, token) = options.identity()?;
    let relay_server = Arc::new(Relay {
        to: options
            .to
            .clone()
            .ok_or_else(|| format!("--to is required\n{USAGE}"))?,
        token: token.clone(),
    });
    let (endpoint, relay_url) = bind_endpoint(&options, key, RELAY_ALPN).await?;
    let (ticket, direct, relays) = mint_ticket(&options, &endpoint, relay_url, token)?;
    eprintln!(
        "caos-iroh: relaying to {} as {}",
        relay_server.to,
        endpoint.addr().id
    );
    if direct.is_empty() {
        eprintln!("caos-iroh: ticket has no direct addresses — every client will be relayed");
    } else {
        eprintln!("caos-iroh: ticket addresses: {}", direct.join(" "));
    }
    eprintln!("caos-iroh: relays: {}", relays.join(" "));
    let url = ticket.to_url();
    eprintln!("caos-iroh: ticket: {url}");

    while let Some(incoming) = endpoint.accept().await {
        let relay_server = relay_server.clone();
        tokio::spawn(async move {
            if let Err(error) = relay_server.accept(incoming).await {
                eprintln!("caos-iroh: relay: connection: {error}");
            }
        });
    }
    Err("the iroh relay endpoint stopped accepting connections".to_string())
}

/// Print the ticket for handing to a client.
///
/// READ FROM THE FILE `serve` WROTE, and DELIBERATELY WITHOUT BINDING an
/// endpoint. The key is the endpoint's identity, so binding a second endpoint on
/// it would publish this process's addresses over the live listener's and make
/// the running server harder to reach — asking for a ticket must not degrade the
/// thing the ticket is for.
///
/// Falls back to an id-only ticket when the file is absent (nothing has served
/// from this state yet). That still resolves through discovery; it just carries
/// no relay hint, so the first connection may take a detour.
fn ticket(options: Options) -> Result<(), String> {
    let path = options.ticket_file()?;
    match std::fs::read_to_string(&path) {
        Ok(url) => {
            println!("{}", url.trim());
            return Ok(());
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("reading {}: {error}", path.display()));
        }
        Err(_) => {}
    }
    let (key, token) = options.identity()?;
    let ticket = Ticket {
        addr: iroh::EndpointAddr::new(key.public()),
        token,
    };
    println!("{}", ticket.to_url());
    Ok(())
}

/// Splice stdin/stdout to one `serve` stream. Two uses: `ext::` remotes
/// (`url = "ext::caos-iroh pipe %s caos://<ticket>"`), and asking a server a
/// question by hand with a request typed into stdin.
async fn pipe(service: &str, url: &str) -> Result<(), String> {
    let service = Service::parse(service)?;
    let ticket = Ticket::parse(url)?;
    let client = caos_iroh::Client::connect(&ticket).await?;
    let git_protocol = std::env::var("GIT_PROTOCOL").ok();
    let (send, recv) = client.open(service, git_protocol.as_deref()).await?;
    splice(send, recv, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Splice stdin/stdout to one `relay` stream — `pipe`'s equivalent for a
/// relay ticket, for testing one by hand.
async fn relay_pipe(url: &str) -> Result<(), String> {
    let ticket = Ticket::parse(url)?;
    let client = caos_iroh::RelayClient::connect(&ticket).await?;
    let (send, recv) = client.open().await?;
    splice(send, recv, tokio::io::stdin(), tokio::io::stdout()).await
}
/// `probe <url> [seconds]` — how does this client reach that server, and does it
/// get better?
///
/// The one question behind every "the ticket is slow" report: a relayed path
/// costs a round trip to the relay on EVERY request, and a run is a chain of
/// them. This keeps ONE connection open and sends a request a second on it,
/// reporting the path whenever it changes, with the time since connect — so "starts relayed and upgrades"
/// and "relayed for ever" tell themselves apart, which no single measurement
/// can.
async fn probe(url: &str, seconds: u64) -> Result<(), String> {
    let ticket = Ticket::parse(url)?;
    let client = caos_iroh::Client::connect(&ticket).await?;
    eprintln!("caos-iroh: probing for {seconds}s, one request a second");
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(seconds);
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        // WITH TRAFFIC ON IT, because an idle connection proves nothing: a path
        // is upgraded while there is something to move.
        match client.open(Service::Http, None).await {
            Ok((send, recv)) => {
                let request = caos_iroh::http::encode_request("GET", "/", &[], None);
                if let Err(error) = caos_iroh::http::exchange(send, recv, "GET", request).await {
                    eprintln!("caos-iroh: request failed: {error}");
                }
            }
            Err(error) => eprintln!("caos-iroh: stream failed: {error}"),
        }
        let now = client.path_description();
        if now != last {
            eprintln!("caos-iroh: +{:.2}s {now}", started.elapsed().as_secs_f64());
            last = now;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    eprintln!("caos-iroh: final: {}", client.path_description());
    client.close().await;
    Ok(())
}
