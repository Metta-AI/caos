# iroh-vhost

Reach a `dumbpipe listen-tcp` target from a browser, `curl` or any HTTP client.
The gateway reads an iroh ticket out of the request, dials that endpoint, and
copies bytes both ways, so keep-alive, streaming and websockets work.

    # server: forward to host:port (IROH_SECRET pins the ticket)
    dumbpipe listen-tcp --host 127.0.0.1:9119

    # client: run the gateway (default 127.0.0.1:8080)
    nix run github:Metta-AI/caos?dir=tools/iroh-vhost -- --host 127.0.0.1:9119

## Where the ticket goes

Exactly one of these three. A request that has it in more than one place is
refused with a 400 saying which, and one with none is refused too.

| place    | example                                         | forwarded as        |
|----------|-------------------------------------------------|---------------------|
| hostname | `http://<label>.<label>.<label>.localhost:8080/path` | path unchanged |
| path     | `http://127.0.0.1:8080/<ticket>/path?q=1`       | `/path?q=1`         |
| header   | `Iroh-Ticket: <ticket>` on any request          | header removed      |

- **Hostname.** `iroh-vhost url <ticket>` prints the URL: the ticket is cut into DNS
  labels of at most 63 characters (253 in total). Browsers and curl resolve
  `*.localhost` to loopback themselves; a client that uses the system resolver
  (Node, anything on glibc without systemd-resolved or nss-myhostname) does not,
  so use one of the other two.
- **Path.** The first segment counts as a ticket if it starts with `endpoint`
  (so `/assets/app.js` is not one). It is taken off every request on the
  connection. A request on that connection without the prefix is forwarded
  unchanged.
- **Header.** For clients that can add a header but cannot change the URL. Page
  assets fetched on new connections carry it too, which the path form cannot do.

## `--host`

Backends that check `Host` (dashboards often do, against DNS rebinding) reject the
gateway's own hostname. `--host 127.0.0.1:9119` sends them that Host instead, on
every request of a connection, and rewrites an `Origin` that matches the Host the
client sent. Bodies pass through unchanged (Content-Length and chunked), and an
`Upgrade` (websocket) request switches the connection to a plain byte copy after
its head.

## Limits

Plain HTTP only. `Location:` headers and other URLs in responses are not
rewritten, so a backend that redirects to its own hostname sends the browser
there. Path mode cannot serve a page whose links open new connections without
the ticket; use the header for that. Websockets through `--host` are untested.

## Debugging

A refused request gets a 400 or 502 with the reason in the body, and the same
line goes to stderr. Failures after the stream opens (the other end hanging up
without a reply) are logged with the close reason. `IROH_VHOST_TRACE=1` adds a
line per request with its byte count; `IROH_VHOST_RELAY_ONLY=1` drops the
ticket's direct addresses, to test the path a remote listener is reached by.

Each TCP connection gets its own iroh connection, because `dumbpipe
listen-tcp` serves one stream per connection and then closes it. Only the
endpoint (the slow part) is kept between requests.
