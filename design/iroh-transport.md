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
    url = caos://<endpoint ticket>.<token>   # id, addresses, relays, token
```

**git reaches it through a remote helper.** A `caos://` URL makes git exec
`git-remote-caos` (gitremote-helpers(7)), which advertises exactly one
capability, `connect`, and needs nothing else: git asks it to connect to
`git-upload-pack` or `git-receive-pack`, it answers with a blank line, and from
then on its stdin and stdout *are* that service's. **There is no pack protocol
in the helper at all** — the server spawns the real service at the far end.

**One connection, one bi-stream per request.** Each stream opens with a request
line naming its service, and the listener answers:

```
caos1 <token> <service>[ <extra>]\n   ->  ok\n  |  err <message>\n
```

`<service>` is `http`, `git-upload-pack` or `git-receive-pack`; `<extra>` is a
`GIT_PROTOCOL` value the listener puts back in the environment of the git it
spawns. The reply is what makes a refusal legible: a revoked token or an
unspawnable service arrives as a message the client prints, not as a closed
stream and git's "the remote end hung up unexpectedly".

A stream per request rather than per connection because the client is
concurrent — `run_chat_turn` holds a long `GET /run` on one thread while the
main thread polls status — and because the object walk asks for one object at a
time by design, so each ask must cost a stream and not a handshake.

## The listener spawns git directly, and that is not a shortcut

`caos-iroh serve` answers a git stream by running `git upload-pack <dir>` /
`git receive-pack <dir>`, with no `http-backend` in front. This looks like it
bypasses the protections the server installs on that object database. It does
not, and the reason is worth keeping: every one of them — `gc.auto=0`,
`receive.autogc=false`, `maintenance.geometric-repack.enabled=false`,
`uploadpack.allowFilter`, the hidden-ref configuration — is written as **git
config on the repo** (`crates/server/src/main.rs`), so it binds whatever process
opens it. The one genuinely http-only setting is `http.receivepack`, which a
direct `receive-pack` never consults.

It is also *less* machinery than the HTTP path: no CGI environment, no
stateless-rpc framing, no chunked response.

## Authentication is the token, not the endpoint id

The ticket carries both. An endpoint id is **discoverable by design** — the
listener publishes its addresses to DNS under it — so "knows the id" cannot be
what authorizes driving a caos server, which runs containers and, per
design/secrets.md, sees every secret. The token is a bearer capability checked
before the listener reads anything else, and it is rotatable without changing
the endpoint id, so revoking a leaked ticket does not invalidate everyone's
remote URL.

The client's own key is ephemeral: there is nothing to enroll, store, or lose. A
node-id allowlist can be layered on later for clients that want stable
identities; the token is what makes that optional rather than urgent.

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

- **git speaks v0 to this helper, and that is git's choice.** The version
  plumbing works — driven by hand with `GIT_PROTOCOL=version=2`, the server
  answers a v2 capability advertisement — but git only offers v2 to a helper
  advertising `stateless-connect`, which its own documentation calls
  experimental and for internal use; a plain `connect` helper is spoken to in
  v0, even under `-c protocol.version=2`. Which costs little here:
  `uploadpack.hideRefs` already keeps `refs/caos/req/` and `refs/caos/res/` out
  of **fetch** advertisements, so the one a fetch reads is small. The
  advertisement that grows without bound is **receive-pack's**, where those refs
  are deliberately visible so a client can negotiate against a server-side
  result — and v2 never applied to push at all. So `stateless-connect` is worth
  having eventually, but it is not on the path of the cost AGENTS.md warns about.

  **And v2 turns out not to need `stateless-connect` at all.** git discovers the
  version from the server's first pkt-line, so it copes with a server that
  volunteers v2 even when told not to ask for it: with `-c protocol.version=0`
  and the listener handed `version=2`, the trace shows `version 2` coming back,
  `ls-refs` running, and the command succeeding. The helper could therefore ask
  for v2 unconditionally. It does not, because the version is the client's to
  choose — someone who set `protocol.version` meant it — but that is a decision,
  not a limit, and it is the cheap way in if the advertisement ever does hurt.

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
  container, so `caosd` passes those in: loopback, plus every address the kernel
  says this host answers to, read from `/proc/net/fib_trie` in pure bash (that
  command is coreutils and bash and nothing else). None of it is inferred about
  the outside world — a host's own addresses are a local fact, and on a machine
  with a public address that is also what a remote client needs, with no
  configuration at all. `CAOS_IROH_ADVERTISE` adds anything else, such as a
  forwarded port on a router.

  Advertising REPLACES the PRIVATE addresses the endpoint discovered and keeps
  the rest (`reachable_from_elsewhere`). Dropping the private ones is what keeps
  the ticket stable — a container gets a new address every time it is created,
  and it names nothing reachable — while keeping the public one matters because
  the relay's view of this machine is exactly what a client on another network
  can use.

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
