//! `git-remote-caos` — the remote helper that makes a ticket a git remote.
//!
//! git execs this for any `caos://…` URL (gitremote-helpers(7): a URL of the
//! form `<transport>://<address>` invokes `git-remote-<transport>` with the URL
//! as its second argument), so the entire client-side configuration of a caos
//! server is one line in the worktree's own `.git/config`:
//!
//! ```text
//! git remote add caos caos://<ticket>
//! ```
//!
//! Which is the point: the ticket is per-worktree state in the place git already
//! keeps per-worktree state. Nothing is shared between two checkouts, so nothing
//! can be fought over.
//!
//! Only the `connect` capability is advertised, and it is all that is needed:
//! git asks us to connect to `git-upload-pack` or `git-receive-pack`, we answer
//! with a blank line, and from then on our stdin/stdout ARE that service's. So
//! there is no pack protocol in here at all — the server spawns the real service
//! at the other end of the stream (see `caos-iroh serve`).

use std::io::{Read, Write};

use caos_iroh::{splice, Client, Service, Ticket};

fn main() {
    if let Err(error) = run() {
        eprintln!("git-remote-caos: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    // git passes <remote name> <url>. The name is ours to ignore; a remote
    // configured with a URL and no name (`git fetch caos://…`) passes the URL
    // for both.
    let _remote = args.next();
    let url = args
        .next()
        .ok_or("usage: git-remote-caos <remote> <caos://ticket> (git invokes this)")?;
    let ticket = Ticket::parse(&url)?;

    loop {
        let command = read_command()?;
        let command = command.trim_end();
        // An empty line (or EOF) ends the conversation.
        if command.is_empty() {
            return Ok(());
        }
        match command.split_once(' ') {
            Some(("connect", service)) => return connect(&ticket, service),
            _ if command == "capabilities" => {
                // `connect` alone: with a smart transport available there is
                // nothing for `fetch`/`push`/`option` to add.
                print!("connect\n\n");
                std::io::stdout()
                    .flush()
                    .map_err(|e| format!("writing capabilities: {e}"))?;
            }
            _ => return Err(format!("unsupported command {command:?}")),
        }
    }
}

/// Read one command line from git WITHOUT reading ahead.
///
/// Byte at a time on purpose. `Stdin::read_line` fills an 8 KiB internal buffer,
/// and the bytes past the line would be the spliced service's first bytes — git
/// waits for our blank-line reply before sending them, so this is belt and
/// braces, but losing them would present as a protocol error with no visible
/// cause.
fn read_command() -> Result<String, String> {
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match handle.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(error) => return Err(format!("reading a command: {error}")),
        }
    }
    String::from_utf8(line).map_err(|_| "command is not UTF-8".to_string())
}

/// Hand our stdio to the remote service.
///
/// The blank line is written only once the stream is open and the server has
/// accepted it, so a bad ticket or a revoked token is reported as an error
/// message here rather than as an unexplained hangup inside git's protocol.
fn connect(ticket: &Ticket, service: &str) -> Result<(), String> {
    let service = Service::parse(service)?;
    // NO WIRE VERSION IS ASKED FOR, and deliberately not read from
    // `GIT_PROTOCOL`: git does not set that for a helper's `connect`, so anything
    // there is the user's shell talking, not git. Measured — `git -c
    // protocol.version=2 ls-remote` over this transport still reads a v0
    // advertisement, because git offers v2 to a helper only over
    // `stateless-connect` (its own docs: experimental, internal use only).
    //
    // Asking for v2 anyway WOULD work, which is worth knowing before anyone
    // writes `stateless-connect`: git discovers the version from the server's
    // first pkt-line, so it copes with a server that volunteers v2 even when
    // configured for v0 (measured: `-c protocol.version=0` with the listener
    // told `version=2` negotiated v2 and succeeded). Not done here, because the
    // version is the CLIENT's to choose — a person who set `protocol.version`
    // means it — and the gain is small on this server, whose fetch
    // advertisements `uploadpack.hideRefs` already trims.
    let git_protocol: Option<&str> = None;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("starting a runtime: {e}"))?;
    runtime.block_on(async move {
        let client = Client::connect(ticket).await?;
        let (send, recv) = client.open(service, git_protocol).await?;
        // Only now: the connection is up and the service is running.
        {
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            stdout
                .write_all(b"\n")
                .map_err(|e| format!("acknowledging connect: {e}"))?;
            stdout
                .flush()
                .map_err(|e| format!("acknowledging connect: {e}"))?;
        }
        let result = splice(send, recv, tokio::io::stdin(), tokio::io::stdout()).await;
        client.close().await;
        result
    })
}
