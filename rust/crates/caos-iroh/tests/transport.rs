//! The transport end to end, over loopback and WITHOUT A RELAY.
//!
//! Both endpoints use the `Minimal` preset and the client is handed the
//! listener's bound address directly, so these tests need no relay, no DNS and
//! no network beyond the loopback interface — which is what makes them ordinary
//! unit tests rather than something that only runs against a live server.
//!
//! What they cover is the part that is easy to get wrong and invisible until a
//! real client arrives: that a stream is a TCP connection to the forwarded port,
//! that a payload survives in both directions at a size larger than any buffer
//! in the path, that HTTP framing completes without the server closing, and that
//! the listener really does speak dumbpipe's protocol — so a client that knows
//! nothing of this crate reaches it.

use std::net::SocketAddr;
use std::sync::Arc;

use caos_iroh::{Client, Server, Ticket, ALPN, HELLO};
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, SecretKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A listener plus the ticket that reaches it.
struct Fixture {
    ticket: Ticket,
    /// Kept alive: dropping the endpoint stops answering.
    _endpoint: Endpoint,
}

async fn listener(to: String) -> Fixture {
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
    let server = Arc::new(Server { to });
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
        },
        _endpoint: endpoint,
    }
}

async fn client_endpoint() -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .bind_addr("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .expect("bind address")
        .bind()
        .await
        .expect("bind the client")
}

async fn client_for(ticket: &Ticket) -> Client {
    Client::connect_on(client_endpoint().await, ticket)
        .await
        .expect("connect to the listener")
}

/// A TCP server standing in for the caos server's HTTP port: reads five bytes,
/// answers one line.
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
async fn a_stream_reaches_the_forwarded_port() {
    let to = echo_line_server().await;
    let fixture = listener(to).await;
    let client = client_for(&fixture.ticket).await;

    let (mut send, mut recv) = client.open().await.expect("open a stream");
    send.write_all(b"ping\n").await.expect("write the request");
    let mut answer = String::new();
    recv.read_to_string(&mut answer)
        .await
        .expect("read the answer");
    assert_eq!(answer, "pong\n");
}

/// THE POINT OF THE PROTOCOL: a client that has never heard of `Client`, with
/// nothing but dumbpipe's ALPN and its handshake, is served. This is what
/// `dumbpipe connect-tcp` and `iroh-vhost` do.
#[tokio::test]
async fn a_stock_dumbpipe_client_is_served() {
    let to = echo_line_server().await;
    let fixture = listener(to).await;
    let endpoint = client_endpoint().await;
    let connection = endpoint
        .connect(fixture.ticket.addr.clone(), b"DUMBPIPEV0")
        .await
        .expect("connect on dumbpipe's ALPN");
    let (mut send, mut recv) = connection.open_bi().await.expect("open a stream");
    send.write_all(b"hello").await.expect("the handshake");
    send.write_all(b"ping\n").await.expect("the request");
    let mut answer = String::new();
    recv.read_to_string(&mut answer)
        .await
        .expect("read the answer");
    assert_eq!(answer, "pong\n");
}

/// One connection, many streams: the listener serves a second stream after the
/// first, which dumbpipe's own `listen-tcp` does not, and which a client that
/// opens a stream per request depends on.
#[tokio::test]
async fn one_connection_carries_many_streams() {
    let to = echo_line_server().await;
    let fixture = listener(to).await;
    let client = client_for(&fixture.ticket).await;
    for round in 0..5 {
        let (mut send, mut recv) = client.open().await.expect("open a stream");
        send.write_all(b"ping\n").await.expect("write the request");
        let mut answer = String::new();
        recv.read_to_string(&mut answer)
            .await
            .expect("read the answer");
        assert_eq!(answer, "pong\n", "round {round}");
    }
}

/// A stream whose handshake is anything but `hello` is dropped, not forwarded:
/// whatever else speaks this ALPN gets nowhere near the backend.
#[tokio::test]
async fn a_stream_without_the_handshake_is_not_forwarded() {
    let to = echo_line_server().await;
    let fixture = listener(to).await;
    let endpoint = client_endpoint().await;
    let connection = endpoint
        .connect(fixture.ticket.addr.clone(), ALPN)
        .await
        .expect("connect");
    let (mut send, mut recv) = connection.open_bi().await.expect("open a stream");
    assert_ne!(HELLO, b"nope!");
    send.write_all(b"nope!ping\n").await.expect("write");
    let answer = tokio::time::timeout(std::time::Duration::from_secs(5), recv.read_to_end(1024))
        .await
        .ok()
        .and_then(|read| read.ok())
        .unwrap_or_default();
    assert!(answer.is_empty(), "the backend answered: {answer:?}");
}

/// A backend that is not there is answered in HTTP, because the stream carries
/// HTTP: the client reads the reason instead of a hangup.
#[tokio::test]
async fn an_unreachable_backend_is_answered_with_a_502() {
    // A port nothing listens on: bind, note it, drop.
    let dead = {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        probe.local_addr().unwrap().to_string()
    };
    let fixture = listener(dead).await;
    let client = client_for(&fixture.ticket).await;

    let (mut send, mut recv) = client.open().await.expect("open a stream");
    send.write_all(b"GET / HTTP/1.1\r\n\r\n")
        .await
        .expect("write");
    let mut answer = String::new();
    recv.read_to_string(&mut answer).await.expect("read");
    assert!(answer.starts_with("HTTP/1.1 502"), "{answer}");
    assert!(answer.contains("connecting to"), "{answer}");
}

/// A server that reads exactly `expect` bytes and then answers with their count.
/// It says nothing until the whole payload has arrived, so a splice that stalls
/// mid-payload deadlocks instead of answering.
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
/// stalled after about 11 KB against a real server.
#[tokio::test]
async fn a_large_payload_travels_up_to_the_forwarded_port() {
    const SIZE: usize = 4 * 1024 * 1024;
    let to = counting_server(SIZE).await;
    let fixture = listener(to).await;
    let client = client_for(&fixture.ticket).await;

    let (mut send, mut recv) = client.open().await.expect("open a stream");
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
    let fixture = listener(to).await;
    let client = client_for(&fixture.ticket).await;

    let (send, recv) = client.open().await.expect("open a stream");
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
