//! The server end of the `caos://` transport, plus a stdio pipe for driving it
//! by hand.
//!
//! ```text
//! caos-iroh serve --state <dir> --to <host:port> [--git-dir <dir>] [--port <n>]
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
    load_or_create_secret, load_or_create_token, splice, Server, Service, Ticket, ALPN,
    ONLINE_TIMEOUT,
};
use iroh::SecretKey;

const USAGE: &str = "\
usage:
  caos-iroh serve --state <dir> --to <host:port> [--git-dir <dir>] [--port <n>]
                  [--advertise <host:port> ...]
  caos-iroh ticket --state <dir>
  caos-iroh pipe <service> <url>
  caos-iroh probe <url> [seconds]

  serve   answer caos:// streams: HTTP ones spliced to --to, git ones served
          from --git-dir, on UDP --port (default 11204; a FIXED port is what
          keeps the ticket stable). Writes the ticket to <dir>/ticket and
          prints it.
          --advertise REPLACES the addresses it finds for itself with ones it
          cannot discover — a published container port, say. Repeatable; it
          never removes the relays, so a client always has a way in.
  ticket  print the ticket, for handing to a client. Reads <dir>/ticket.
  probe   report how a client reaches that server, once a second: a RELAY path
          costs a round trip to the relay on every request.
  pipe    splice stdin/stdout to one stream (<service> is http,
          git-upload-pack or git-receive-pack). What `ext::` remotes use.
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

async fn serve(options: Options) -> Result<(), String> {
    let (key, token) = options.identity()?;
    let server = Arc::new(Server {
        to: options
            .to
            .clone()
            .ok_or_else(|| format!("--to is required\n{USAGE}"))?,
        git_dir: options.git_dir.clone(),
        token,
    });
    let mut builder = caos_iroh::endpoint_builder()
        .secret_key(key)
        .alpns(vec![ALPN.to_vec()]);
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

    // Wait for our own addresses before printing a ticket, so the one we print
    // carries a relay a client can reach us through immediately rather than
    // relying on DNS discovery. Not fatal if it times out: the endpoint id in
    // the ticket resolves on its own, it just may take a first-connection
    // detour.
    if tokio::time::timeout(ONLINE_TIMEOUT, endpoint.online())
        .await
        .is_err()
    {
        eprintln!("caos-iroh: warning: no relay yet; the ticket may take longer to resolve");
    }
    // ADDRESSES INCLUDED, which is the difference between a direct path and a
    // relay round trip on every request (see `caos_iroh::DEFAULT_PORT`). A hint
    // that goes stale — a machine that changed networks — costs a failed probe
    // and falls back to the relay, so a ticket outlives the addresses in it.
    //
    // `--advertise` REPLACES what the endpoint found for itself, rather than
    // adding to it, and both halves of that matter.
    //
    // It is needed at all because an endpoint's view of itself can be wrong: in a
    // container on a rootless podman network it discovers only its own
    // `10.89.x.y`, which has no route to it from anywhere — not even the host —
    // while the address that DOES reach it is the host's, forwarded by a
    // published port. Something outside has to say so.
    //
    // Advertising REPLACES the PRIVATE half of what was discovered and keeps the
    // rest (`reachable_from_elsewhere`). Both halves of that matter:
    //
    // * a container's own `10.89.x.y` is unreachable from anywhere and changes
    //   every time the container is created, so keeping it would put a value in
    //   the ticket that moves on every `caosd up` while naming nothing — the
    //   durability problem that pinning the port was meant to end;
    // * the public address the relay observed is the one a client on another
    //   network needs, and dropping it would send every remote client through a
    //   relay for ever.
    //
    // The relays stay either way, so a client that can use none of these
    // addresses still connects.
    // THE CONFIGURED RELAY GOES IN WHETHER OR NOT IT HAS BEEN REACHED. `addr()`
    // reports relays this endpoint has CONNECTED to, so a caosd brought up with
    // no route out -- an ordinary thing to do -- mints a ticket with no relay in
    // it: usable from its own LAN and nowhere else, and nothing in the ticket
    // says so. `CAOS_IROH_RELAY` is a statement of intent, so it is trusted
    // here and the connection can happen whenever the network does.
    let discovered = match caos_iroh::configured_relay() {
        Some(relay) if !endpoint.addr().relay_urls().any(|u| *u == relay) => {
            endpoint.addr().with_relay_url(relay)
        }
        _ => endpoint.addr(),
    };
    let addr = if options.advertise.is_empty() {
        discovered
    } else {
        let mut addr = iroh::EndpointAddr::new(discovered.id);
        for relay in discovered.relay_urls() {
            addr = addr.with_relay_url(relay.clone());
        }
        for kept in discovered
            .ip_addrs()
            .filter(|a| caos_iroh::reachable_from_elsewhere(a))
        {
            addr = addr.with_ip_addr(*kept);
        }
        for extra in &options.advertise {
            addr = addr.with_ip_addr(*extra);
        }
        addr
    };
    // REPORTED FROM THE TICKET, not from the endpoint: with `--advertise` the two
    // differ, and the ticket's set is the one clients act on. An empty one means
    // every client is relayed wherever it sits — worth saying out loud, because
    // the only other symptom is that everything is slow.
    let direct: Vec<String> = addr.ip_addrs().map(|a| a.to_string()).collect();
    let relays: Vec<String> = addr.relay_urls().map(|u| u.to_string()).collect();
    let url = Ticket {
        addr,
        token: server.token.clone(),
    }
    .to_url();
    // WRITTEN BEFORE THE FIRST ACCEPT, so whatever brought this process up can
    // wait on the file and then hand the ticket out. 0600 like the token it
    // contains — a ticket IS the capability, so a world-readable copy of it
    // hands this server to every account on the machine.
    let path = options.ticket_file()?;
    caos_iroh::write_private(&path, &format!("{url}\n"), true)?;
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

/// Splice stdin/stdout to one stream. Two uses: `ext::` remotes
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
