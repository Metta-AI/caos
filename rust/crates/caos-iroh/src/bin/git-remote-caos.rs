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
//! WHAT THIS DOES IS LET GIT SPEAK HTTP. The server's git is smart HTTP at
//! `/info/refs`, `/git-upload-pack` and `/git-receive-pack`, like everything else
//! it serves, so this helper holds no pack protocol at all. It listens on a
//! loopback port, forwards every connection git's own HTTP helper makes to it
//! over the ticket (one stream each, on one iroh connection), and runs
//! `git remote-http` against that port with git's stdio. git's HTTP transport
//! negotiates protocol v2, inflates what it compresses and chunks what it
//! streams, none of which is ours to reimplement.
//!
//! Over HTTP is also the route that carries the server's push checks (the
//! boundary validation in `server/src/git.rs`), which a direct `receive-pack`
//! on the object database never did.

use std::process::Command;
use std::sync::Arc;

use caos_iroh::{splice, Client, Ticket};
use tokio::net::TcpListener;
use tokio::sync::OnceCell;

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("git-remote-caos: {error}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32, String> {
    let mut args = std::env::args().skip(1);
    // git passes <remote name> <url>. A remote configured with a URL and no name
    // (`git fetch caos://…`) passes the URL for both.
    let remote = args.next().ok_or(USAGE)?;
    let url = args.next().ok_or(USAGE)?;
    let ticket = Ticket::parse(&url)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("starting a runtime: {e}"))?;

    // Bound here, before the child exists, so the port it is told is already
    // listening. The iroh connection is made when git first uses it, not now:
    // `capabilities` and `list` do not need it to be up, and a bad ticket is
    // then reported by git's own request failing with our message on stderr.
    let listener = runtime
        .block_on(TcpListener::bind("127.0.0.1:0"))
        .map_err(|e| format!("listening on loopback: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("reading the loopback port: {e}"))?
        .port();
    runtime.spawn(forward(listener, ticket));

    let mut command = git_remote_http();
    command
        .arg(&remote)
        .arg(format!("http://127.0.0.1:{port}/"))
        // A proxy variable must not capture a loopback connection.
        .env("no_proxy", "127.0.0.1")
        .env("NO_PROXY", "127.0.0.1");
    let status = command
        .status()
        .map_err(|e| format!("running git remote-http: {e}"))?;
    Ok(status.code().unwrap_or(1))
}

const USAGE: &str = "usage: git-remote-caos <remote> <caos://ticket> (git invokes this)";

/// The `git remote-http` to run. git sets `GIT_EXEC_PATH` for its helpers, which
/// is where the sibling helper is; `git` on PATH is the fallback for a person
/// running this by hand.
fn git_remote_http() -> Command {
    if let Some(dir) = std::env::var_os("GIT_EXEC_PATH") {
        let path = std::path::Path::new(&dir).join("git-remote-http");
        if path.exists() {
            return Command::new(path);
        }
    }
    let mut command = Command::new("git");
    command.arg("remote-http");
    command
}

/// Accept connections from git's HTTP helper and give each its own stream.
async fn forward(listener: TcpListener, ticket: Ticket) {
    let client: Arc<OnceCell<Client>> = Arc::new(OnceCell::new());
    let ticket = Arc::new(ticket);
    loop {
        let (socket, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                eprintln!("git-remote-caos: accepting: {error}");
                return;
            }
        };
        let (client, ticket) = (client.clone(), ticket.clone());
        tokio::spawn(async move {
            // ONE connection for the life of this process, dialled by whichever
            // request comes first; every other request is a stream on it.
            let client = match client.get_or_try_init(|| Client::connect(&ticket)).await {
                Ok(client) => client,
                Err(error) => {
                    eprintln!("git-remote-caos: {error}");
                    return;
                }
            };
            let (send, recv) = match client.open().await {
                Ok(streams) => streams,
                Err(error) => {
                    eprintln!("git-remote-caos: {error}");
                    return;
                }
            };
            let (read, write) = socket.into_split();
            if let Err(error) = splice(send, recv, read, write).await {
                eprintln!("git-remote-caos: {error}");
            }
        });
    }
}
