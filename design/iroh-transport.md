# Reaching a server by ticket: the `caos://` transport — design note

**Status:** implemented and verified against a live stack. A worktree whose only
server configuration is a ticket fetches, pushes, and runs:

```
caosd up --iroh          # on the server's machine
caosd ticket             # the one string a client needs

git remote add caos caos://<ticket>      # on the client
caos-cli run --base:@=std/hello --greeting=hi --who=ticket
```

`crates/caos-iroh` carries the transport, `caos-iroh serve` is the listener (a
stack member under `CAOS_STACK_IROH`), `git-remote-caos` is the remote helper,
and `caos-iroh::http` is the client half — every `/object`, `/run`, `/status`,
`/sub-run` and `/trace/child` call in `crates/caos` now goes through one
`server_request` that dispatches on the scheme.

Measured over a ticket against the stack on this machine: a cached `run` ~100ms
(87ms over local HTTP, so within 15%), a direct path at 69µs, a cold
(salted) one 14.3s. Getting there needed a fixed UDP port and `--advertise` —
see "what actually went wrong" below before changing either.

## Problem

Reaching a caos server from another machine meant giving that machine a route:
a DNS name, a VPN, a published port. We want the inverse — a server anywhere, a
ticket copied to a client, and nothing else configured.

The obvious shape is a tunnel: an iroh listener beside the server, a local TCP
port on the client, and the existing HTTP URL pointed at `127.0.0.1`. That was
measured end to end with `dumbpipe` and works (`git push`, a 30 MB fetch and a
full `caos-cli run` all pass through it unchanged). It was rejected anyway, for
two reasons that are not about iroh:

- **A long-running client process.** Something has to hold the tunnel open, and
  its lifetime is not any one command's.
- **A port is global.** Two worktrees pointed at two servers fight over it, and
  so do two people on one machine. The port number becomes shared state
  precisely where this tree keeps none — compare `/caos-dev/bin`, one fixed
  path under a shared volume being one stack's answer for all of them
  (AGENTS.md, "Several dev stacks at once").

## Shape

**The ticket lives in the git remote's URL**, which is per-worktree state in the
place git already keeps per-worktree state. Two checkouts share nothing, so
there is nothing to collide over:

```
[remote "caos"]
    url = caos://<endpoint ticket>   # id, addresses, relays: what dumbpipe prints
```

**The protocol is dumbpipe's, and nothing else.** The listener answers the ALPN
`DUMBPIPEV0`; an opener writes the five bytes `hello` on a fresh bi-stream, and
from then on that stream is a TCP connection to the one port the listener
forwards to, the caos server's HTTP port. There is no request line, no service
to name and no reply. So a ticket from `caos-iroh serve` is a dumbpipe ticket,
and stock `dumbpipe connect-tcp` or `tools/iroh-vhost` reach the server with it.

**Everything the server offers is a URL path**, and that is what made this
possible: `/object`, `/run`, `/eval`, `/start`, `/result`, `/mcp`, and git smart
HTTP at `/info/refs`, `/git-upload-pack` and `/git-receive-pack`. The listener
needs no idea what any of them are.

**One connection, one bi-stream per request.** The one thing the listener does
beyond dumbpipe's own is keep a connection open and answer many streams on it
(stock `dumbpipe listen-tcp` serves one and closes, which costs a QUIC
handshake per request). A stock client sends one stream and loses nothing; ours
holds one connection and opens a stream per request, because the client is
concurrent — `run_chat_turn` holds a long `GET /run` on one thread while the
main thread polls status — and because the object walk asks for one object at a
time by design, so each ask must cost a stream and not a handshake.

A backend the listener cannot dial is answered with an HTTP 502 on the stream,
naming it, because the stream carries HTTP and a reason is better than a hangup.

## git is smart HTTP, through git's own transport

`git-remote-caos` holds no pack protocol and no longer asks the server for a
direct `upload-pack`. It listens on a loopback port, forwards every connection
git's own HTTP helper makes to it over the ticket (one stream each, on one iroh
connection), and runs `git remote-http` against that port with git's stdio. A
`caos://` URL still makes git exec the helper (gitremote-helpers(7)), so nothing
that uses a ticket as a remote changed.

This replaced a listener that spawned `git upload-pack` / `git receive-pack`
directly. That was not unsafe — every protection the server installs on the
object database is git config on the repo — but it was a second way in, and it
skipped what the HTTP path adds: `receive-pack`'s boundary validation, the
refusal of shallow and encoded pushes (`crates/server/src/git.rs`). Going
through HTTP means a push over a ticket is checked exactly like a push over
`http://localhost`. git's own HTTP transport also negotiates protocol v2 and
inflates what it compresses, so the helper has nothing to volunteer or decode.

## Authentication is the ticket

There is none beyond it. The endpoint id is a public key, and knowing it, with a
relay or an address, is the whole credential, as with `dumbpipe`. An earlier
version added a shared token to every stream, checked before anything else and
rotatable without changing the id. It was dropped on purpose: it made the
protocol ours alone, and a ticket is already something to keep private. Treat one
as you would a password to the server, which runs containers and, per
design/secrets.md, sees every secret. n0's discovery is not used, so an id is not
published anywhere unless the ticket leaks. Old `caos://<ticket>.<token>` URLs
still parse; the suffix is ignored.

The client's own key is ephemeral: there is nothing to enroll, store, or lose.

## What actually went wrong, so it is not rediscovered

Two bugs, both invisible to everything that had been tested at the time, and both
with the same shape: a small answer works, a real one hangs.

- **A response must be framed by its headers, not by end-of-stream.** Reading to
  EOF and parsing afterwards passes every small answer. The caos server hands
  tiny_http `data_length = None` for anything it streams, so objects and
  pack-sized bodies come back `Transfer-Encoding: chunked` and the socket is *not*
  closed after the terminating chunk. The symptom was a `caos-cli run` that
  printed nothing and never returned, while the server log showed its requests
  being answered one after another. The reader now stops at the chunked
  terminator or `Content-Length`. Related: a **HEAD** response carries the
  `Content-Length` of a body it does not send, and waiting for those bytes made
  `server_holds` answer "no" for objects the server holds.

- **An EOF from the client must CLOSE the git service's stdin, and
  `shutdown()` does not.** `tokio::process::ChildStdin::shutdown` flushes and
  returns; the pipe closes on drop. So when `git pack-objects` failed locally —
  "unable to read <oid>", the ordinary case of pushing a request whose base the
  client cannot read (`tests/push-closure`) — git closed its side, `receive-pack`
  went on waiting for a pack for ever, and git waited for a report that would
  never come. Over HTTP this case fails in milliseconds, because the server drops
  `receive-pack`'s stdin when the request body ends (`git.rs` says so), and
  `caos-cli` then falls back to handing objects over one at a time. The fix is
  that `splice` now MOVES each local half into its direction, so finishing a
  direction drops it. `tests/transport.rs` holds the regression test, and it was
  checked against the bug: restore the leak and it times out.

## Five things that were measured, not reasoned

- **A graceful close costs about a second, flat.** `Endpoint::close` waits long
  enough for the CONNECTION_CLOSE frame to be very likely delivered, and does
  not return early when it is: `git ls-remote` over this transport took 1.02s
  with it and 0.023s without. The helper runs once per git command, so that is
  paid per command. It is capped at 25 ms now, which is enough for the transmit
  in practice (the listener logs `closed by peer`, not a timeout).

- **git over a ticket is git over HTTP now**, so the version question that used
  to live here is git's own: its HTTP transport sends `Git-Protocol: version=2`,
  the server forwards it to `http-backend` (`git.rs`), and `push.negotiate` has
  the v2 `fetch --negotiate-only` it needs. The direct-spawn helper had to
  volunteer v2 and could not be asked; that workaround, and the v0 advertisement
  cost it was written around, are gone.

- **A ticket with NO addresses never upgrades to a direct path, and that cost
  46x.** A relayed connection is supposed to become direct within a second, and
  does. What stops it is a ticket carrying no address at all — which this
  transport briefly minted, for a reason that was half right: a listener on an
  ephemeral UDP port prints a different ticket after every restart, because the
  port in its addresses moved. Stripping the addresses made the text stable and
  made every client relayed for ever.

  Measured, and worth keeping because it is so lopsided. Ticket with **any**
  address — including `203.0.113.1`, a TEST-NET address nothing can reach —
  upgrades to direct in **0.10 s**, having learned the server's real address over
  the relay. Ticket with none: still relayed after 45 s of traffic, one path
  open, never a second. And a relayed path costs a round trip to the relay on
  EVERY request, where a run is a chain of them: `caos-cli run --base:@=std/hello`
  against a server ON THE SAME MACHINE took 4.6 s — 32 sequential requests at
  ~135 ms — against 63 ms over HTTP.

  So the fix is to make the addresses stable rather than to remove them: a FIXED
  UDP port (`caos_iroh::DEFAULT_PORT`, 11204). Same command afterwards: **~100
  ms**, direct at 70 µs, against 87 ms over local HTTP.

- **An endpoint in a container cannot discover the address that reaches it, and
  cannot punch its way out either.** With rootless podman the container network
  is user-mode: the listener discovers only its own `10.89.x.y`, the host has no
  route to it (`/proc/net/route` has eth0 and a default, nothing else), and it
  receives no unsolicited inbound UDP. Probed for 45 s with traffic flowing, that
  connection never left the relay — the one placement where patience genuinely
  does not help.

  What reaches it is THIS MACHINE's address with the UDP port published into the
  container, and only something outside the container knows it. `caosd` passes
  through whatever `CAOS_IROH_ADVERTISE` names and nothing else, so the direct
  set in a ticket is exactly what an operator stated.

  NOTHING IS INFERRED, and that is the point. This used to seed loopback and add
  every address the kernel said the host answered to, read from
  `/proc/net/fib_trie`. On a LAN that is a useful local fact; behind NAT it is
  four wrong answers and one right one. Measured on the caosd-prod box: the
  sweep produced the VPC address, the `docker0` gateway and the gateway of
  `caos-net` -- a bridge caos creates itself -- while only the Elastic IP
  reached anything. The caos-net gateway moves when that bridge is recreated, so
  a ticket for a server whose identity had not changed changed with it;
  `127.0.0.1` names the client's own machine; a client on a colliding 10.x range
  probes its neighbours; and every holder of a ticket learned the internal
  addressing.

  An endpoint's own discovered addresses are not used either, for the same
  reason they were being overridden: in a container it sees only its container
  address, which reaches it from nowhere and changes on every create. With
  nothing advertised a ticket carries only relays, and every client is relayed
  -- the honest result when no one has said which address reaches the host.

## What this does not fix

The object protocol is still one round trip per object: `checkout` asks for the
root and then only for the children it lacks, deliberately (see the long comment
in `GitTransport::get_object`), which was designed when the server was "a
low-latency hop away". Over a WAN that is the thing that will hurt, and no
transport choice changes it — the fix is a batch endpoint, independent of this
note. For the same reason `ensure_pushed`'s `HEAD /object/<hash>` probe, which
exists to avoid receive-pack's ever-growing ref advertisement, matters much more
over a ticket than over a docker network.

## Three things the client half needed that a reader would otherwise undo

- **The transport is INSTALLED, not linked.** `crates/caos` declares a
  `TicketTransport` trait and a `OnceLock`; `caos-cli` installs an
  implementation backed by `caos-iroh` in `main`. That indirection exists for one
  reason: `crates/caos` is also the worker's setuid `/bin/caos`, and cargo
  unifies features across workspace members in one `cargo build --workspace`, so
  *depending* on the iroh crate there — however carefully gated — would bake a
  QUIC stack into every worker image. Checked rather than assumed: the built
  `/bin/caos` contains **no** `iroh` strings, while `caos-cli` and
  `git-remote-caos` contain thousands. (It does carry `rustls`, and always did —
  the bake anchor declares `minreq` with `https-rustls`, and that unification
  reaches the worker. Which is the same effect, demonstrated, and the reason this
  crate is kept out of that graph.)

- **`caos-cli` puts its own directory on PATH** (`ensure_helper_on_path`) before
  shelling out to git, because a `caos://` remote is served by a helper git execs
  by name. Without it a run reaches the server itself and then dies inside git
  with `'remote-caos' is not a git command`. Derived from `current_exe`, so it
  holds for a cargo target dir, a nix store path and a copy alike — and
  `caos-cli-bin` ships the helper in that same directory for it to find.

- **Iroh lands in the shared musl deps bake** (`std/cargo/bake.nix`), because on
  Linux the host `caos-cli` *is* the workspace build. A one-time re-bake, on the
  host and again inside the stack container, and a larger builder image.

## Remaining work

1. **Only `caos-cli` installs the transport.** Anything else that talks to a
   server over HTTP — `caosd`'s probes, `stack/build-builtins.sh`, the seeder —
   still needs an address. That is right for now (they all run beside the server)
   but it means "a caos:// server" is a client-side notion only.

2. **The listener runs INSIDE the stack container**, which is the simple
   placement: one lifetime, one log directory, and the git dir right there. It is
   also behind docker's NAT, so expect relayed connections rather than
   hole-punched ones. A host-side placement would traverse better and can reach
   the same repo (`$CAOS_DATA/stack/git` is bind-mounted), at the cost of its own
   supervision.

3. **The object walk is still one round trip per object** (see "What this does
   not fix"), and that is the next thing worth measuring over a real WAN link.
