//! The transport end to end, over loopback and WITHOUT A RELAY.
//!
//! Both endpoints use the `Minimal` preset and the client is handed the
//! listener's bound address directly, so these tests need no relay, no DNS and
//! no network beyond the loopback interface — which is what makes them ordinary
//! unit tests rather than something that only runs against a live server.
//!
//! What they cover is the part that is easy to get wrong and invisible until a
//! real client arrives: that a stream carries its service to the right handler,
//! that a bad token is refused with a message instead of a hangup, and that a
//! spliced payload survives in both directions.

use std::net::SocketAddr;
use std::sync::Arc;

use caos_iroh::{hex_encode, Client, Server, Service, Ticket, ALPN};
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, SecretKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A listener plus the ticket that reaches it.
struct Fixture {
    ticket: Ticket,
    /// Kept alive: dropping the endpoint stops answering.
    _endpoint: Endpoint,
}

async fn listener(to: String, git_dir: Option<std::path::PathBuf>) -> Fixture {
    let token = hex_encode(&[0x5au8; 32]);
    let key = SecretKey::from_bytes(&[3u8; 32]);
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(key.clone())
        .alpns(vec![ALPN.to_vec()])
        .bind_addr("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .expect("bind address")
        .bind()
        .await
        .expect("bind the listener");
    let bound = *endpoint.bound_sockets().first().expect("a bound socket");
    let server = Arc::new(Server {
        to,
        git_dir,
        token: token.clone(),
    });
    let accepting = endpoint.clone();
    tokio::spawn(async move {
        while let Some(incoming) = accepting.accept().await {
            let server = server.clone();
            tokio::spawn(async move {
                let _ = server.accept(incoming).await;
            });
        }
    });
    Fixture {
        ticket: Ticket {
            addr: EndpointAddr::new(key.public()).with_ip_addr(bound),
            token,
        },
        _endpoint: endpoint,
    }
}

async fn client_for(ticket: &Ticket) -> Client {
    let endpoint = Endpoint::builder(presets::Minimal)
        .bind_addr("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .expect("bind address")
        .bind()
        .await
        .expect("bind the client");
    Client::connect_on(endpoint, ticket)
        .await
        .expect("connect to the listener")
}

/// A TCP server standing in for the caos server's HTTP port: reads a line,
/// answers one.
async fn echo_line_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the echo server");
    let addr = listener.local_addr().expect("local addr").to_string();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 5];
                if stream.read_exact(&mut request).await.is_ok() {
                    let _ = stream.write_all(b"pong\n").await;
                }
                let _ = stream.shutdown().await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn an_http_stream_reaches_the_forwarded_port() {
    let to = echo_line_server().await;
    let fixture = listener(to, None).await;
    let client = client_for(&fixture.ticket).await;

    let (mut send, mut recv) = client
        .open(Service::Http, None)
        .await
        .expect("open an http stream");
    send.write_all(b"ping\n").await.expect("write the request");
    let mut answer = String::new();
    recv.read_to_string(&mut answer)
        .await
        .expect("read the answer");
    assert_eq!(answer, "pong\n");
}

#[tokio::test]
async fn a_wrong_token_is_refused_with_a_message() {
    let to = echo_line_server().await;
    let fixture = listener(to, None).await;
    // Same endpoint, a token that is not its own: the shape of a stale ticket
    // after a rotation.
    let stale = Ticket {
        addr: fixture.ticket.addr.clone(),
        token: hex_encode(&[0xffu8; 32]),
    };
    let client = client_for(&stale).await;

    let error = client
        .open(Service::Http, None)
        .await
        .expect_err("refused before any payload");
    assert!(error.contains("unauthorized"), "{error}");
}

#[tokio::test]
async fn a_git_service_is_refused_when_the_listener_serves_no_repo() {
    let to = echo_line_server().await;
    let fixture = listener(to, None).await;
    let client = client_for(&fixture.ticket).await;

    let error = client
        .open(Service::GitUploadPack, Some("version=2"))
        .await
        .expect_err("no git dir");
    assert!(error.contains("serves no git"), "{error}");
}

/// A server that reads exactly `expect` bytes and then answers with their count.
/// Stands in for `git receive-pack`: it says nothing until the whole payload has
/// arrived, so a splice that stalls mid-payload deadlocks instead of answering.
async fn counting_server(expect: usize) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the counting server");
    let addr = listener.local_addr().expect("local addr").to_string();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut payload = vec![0u8; expect];
                let read = stream.read_exact(&mut payload).await;
                let answer = match read {
                    Ok(n) => format!("read {n}\n"),
                    Err(error) => format!("failed {error}\n"),
                };
                let _ = stream.write_all(answer.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });
    addr
}

/// A payload the client sends UP, larger than any buffer in the path.
///
/// This is the direction nothing else covered, and the direction that broke: a
/// fetch pulls megabytes down and worked from the first try, while a push — the
/// client writing a pack while the server says nothing until it has all of it —
/// stalled after about 11 KB against a real server. The listener sits between
/// two independent flows here (stream to child stdin, child stdout to stream),
/// and this is what proves neither starves the other.
#[tokio::test]
async fn a_large_payload_travels_up_to_the_forwarded_port() {
    const SIZE: usize = 4 * 1024 * 1024;
    let to = counting_server(SIZE).await;
    let fixture = listener(to, None).await;
    let client = client_for(&fixture.ticket).await;

    let (mut send, mut recv) = client
        .open(Service::Http, None)
        .await
        .expect("open a stream");
    let payload = vec![b'x'; SIZE];
    let answer = tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        send.write_all(&payload).await.expect("write the payload");
        let mut answer = String::new();
        recv.read_to_string(&mut answer)
            .await
            .expect("read the answer");
        answer
    })
    .await
    .expect("the payload is not held up mid-flight");
    assert_eq!(answer, format!("read {SIZE}\n"));
}

/// An HTTP server that answers chunked and then KEEPS THE CONNECTION OPEN, which
/// is what the caos server does for anything it streams. Returns its address.
///
/// The hold is the point: a client that framed the body by waiting for
/// end-of-stream would hang here, which is exactly how this failed the first time
/// against a real server — a `caos-cli run` that printed nothing and never
/// returned. The task keeps the socket alive until the test ends.
async fn chunked_server(body: &'static str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the chunked server");
    let addr = listener.local_addr().expect("local addr").to_string();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = [0u8; 1];
            // Wait for the request to start before answering.
            let _ = stream.read_exact(&mut request).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
            held.push(stream);
        }
    });
    addr
}

#[tokio::test]
async fn a_chunked_response_completes_without_the_server_closing() {
    let to = chunked_server("hello over a ticket").await;
    let fixture = listener(to, None).await;
    let client = client_for(&fixture.ticket).await;

    let (send, recv) = client
        .open(Service::Http, None)
        .await
        .expect("open an http stream");
    let request = caos_iroh::http::encode_request("GET", "/object/abc", &[], None);
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        caos_iroh::http::exchange(send, recv, "GET", request),
    )
    .await
    .expect("the response is framed, so this cannot hang")
    .expect("a response");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"hello over a ticket");
}

/// The real thing: `git ls-remote` over the transport, against a repo made here.
///
/// Skipped rather than failed when git is absent, because this crate's other
/// tests do not need it and the environments that run them vary; the assertions
/// above still cover the protocol.
#[tokio::test]
async fn git_upload_pack_advertises_a_ref_over_the_transport() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("no git on PATH: skipping the upload-pack test");
        return;
    }
    let repo = tempfile::tempdir().expect("a temp dir");
    let git_dir = repo.path().join("origin.git");
    let status = std::process::Command::new("git")
        .args(["init", "--quiet", "--bare"])
        .arg(&git_dir)
        .status()
        .expect("run git init");
    assert!(status.success(), "git init failed");

    let to = echo_line_server().await;
    let fixture = listener(to, Some(git_dir)).await;
    let client = client_for(&fixture.ticket).await;

    let (mut send, mut recv) = client
        .open(Service::GitUploadPack, None)
        .await
        .expect("open an upload-pack stream");
    // A v0 advertisement arrives unprompted; the four bytes are the first
    // pkt-line's hex length. An empty repo answers the "no refs" capability
    // line, which is enough to prove git ran and its output came back.
    let mut length = [0u8; 4];
    recv.read_exact(&mut length).await.expect("read a pkt-line");
    let length = std::str::from_utf8(&length).expect("ascii");
    assert!(
        u32::from_str_radix(length, 16).is_ok(),
        "not a pkt-line length: {length:?}"
    );
    // End the conversation the way git does — a flush packet meaning "no wants"
    // — and drain to EOF. Without this the test drops the stream mid-protocol
    // and upload-pack prints "the remote end hung up unexpectedly" into the test
    // output: true, but our doing, and it would mask a real one.
    send.write_all(b"0000").await.expect("write a flush");
    send.finish().expect("finish the stream");
    let mut rest = Vec::new();
    let _ = tokio::io::AsyncReadExt::read_to_end(&mut recv, &mut rest).await;
}

/// A push that sends NO PACK must be answered, not waited on.
///
/// The regression test for a real hang. `git pack-objects` fails locally whenever
/// a client pushes a request whose base it cannot read (AGENTS.md,
/// `tests/push-closure`) — git then closes its side without sending a pack, and
/// the server has to notice. It did not: `shutdown()` on a
/// `tokio::process::ChildStdin` flushes without closing the pipe, so
/// `receive-pack` waited for a pack for ever while git waited for its report, and
/// `caos-cli` never reached the object-by-object fallback that makes such a push
/// succeed. A `caos-cli run` simply never returned.
#[tokio::test]
async fn a_push_that_ends_without_a_pack_is_answered() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("no git on PATH: skipping the receive-pack test");
        return;
    }
    let repo = tempfile::tempdir().expect("a temp dir");
    let git_dir = repo.path().join("target.git");
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet", "--bare"])
            .arg(&git_dir)
            .status()
            .expect("run git init")
            .success(),
        "git init failed"
    );

    let to = echo_line_server().await;
    let fixture = listener(to, Some(git_dir)).await;
    let client = client_for(&fixture.ticket).await;

    let (mut send, mut recv) = client
        .open(Service::GitReceivePack, None)
        .await
        .expect("open a receive-pack stream");
    let answer = tokio::time::timeout(std::time::Duration::from_secs(20), async move {
        // Read the advertisement, ask to create a ref, then end our side with no
        // pack at all — exactly what git does after pack-objects fails.
        let mut advertisement = [0u8; 4];
        recv.read_exact(&mut advertisement)
            .await
            .expect("an advertisement");
        // One create command as a pkt-line (the 4-hex length counts itself),
        // then a flush packet — the point where a pack would follow.
        let command = format!(
            "{} {} refs/caos/req/x\0report-status\n",
            "0".repeat(40),
            "1".repeat(40)
        );
        let request = format!("{:04x}{command}0000", command.len() + 4);
        send.write_all(request.as_bytes()).await.expect("write");
        send.finish().expect("end our side without a pack");
        let mut report = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut recv, &mut report).await;
        report
    })
    .await
    .expect("the server answers rather than waiting for a pack that is not coming");
    // What it says is git's business; that it said anything and the stream ended
    // is the property under test.
    assert!(
        !answer.is_empty(),
        "receive-pack ended the stream with no report at all"
    );
}
