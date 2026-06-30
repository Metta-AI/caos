# Fly backend — implementation sketch

**Status:** design sketch, not a compiling patch. Function signatures track the
real code (`crates/caos/src/bin/caos.rs`, `crates/server/src/compute.rs`,
`crates/worker-common/src/lib.rs`) so it drops in, but HTTP plumbing and the fly
API calls are pseudocode. Everything is gated behind a backend switch so the
existing `docker run` path stays the default for local/tilt dev.

## Topology recap

- **caosd** — one always-on fly app + a Volume holding the bare git repo (`/git`).
  `min_machines_running >= 1`, no autostop. Sheds the docker socket; instead it
  *dispatches over HTTP* and *provisions over the Machines API*.
- **worker version H** — its own scale-to-zero fly app `caos-worker-<H>` with N
  pre-created machines, `hard_limit = 1`, `min_machines_running = 0`.
- **wiring** — workers reach the server at `http://caos-server.internal`
  (direct 6PN); the server reaches workers at `http://caos-worker-<H>.flycast`
  (proxy → autostart + load-balance + replay). All one org, one region.

The worker no longer runs as a one-shot container per request. It runs `caos
serve` (an HTTP server), and the server POSTs jobs to it. `serve` forks the
**unchanged** `entrypoint` per job, one at a time — so each job's lifecycle
(`/cas` wipe → fetch → run → teardown) is bit-for-bit what it is today.

---

## Backend switch (config)

```rust
// crates/server/src/main.rs — Config
enum Backend {
    Docker,                 // today: `docker run` per request (local/tilt)
    Fly { org: String, region: String, token: String, machines_per_worker: u32 },
}
// env: CAOS_BACKEND=docker|fly, CAOS_FLY_ORG, CAOS_FLY_REGION,
//      CAOS_FLY_TOKEN (org-scoped deploy token), CAOS_FLY_POOL=N
```

---

## Piece 1 — `caos serve` (worker side)

New subcommand in `crates/caos/src/bin/caos.rs`. The single execution slot is the
whole busy-tracking mechanism; threads exist only so a busy worker can answer
"replay" (or hold a blocked connection) without stalling `accept()`.

```rust
// One job at a time. Held for the lifetime of a job AND its cleanup, so the
// slot never frees until the VM is clean again.
static SLOT: Mutex<()> = Mutex::new(());

struct Job { args: String, std: String, salt: String, stack: String }

fn serve() -> Result<(), String> {
    let listener = TcpListener::bind("[::]:8080")
        .map_err(|e| format!("binding :8080: {e}"))?;       // flycast hits :8080
    for conn in listener.incoming().flatten() {
        std::thread::spawn(move || {
            if let Err(e) = handle(conn) { eprintln!("serve: {e}"); }
        });
    }
    Ok(())
}

fn handle(mut conn: TcpStream) -> Result<(), String> {
    // read_request returns the parsed job plus whether the proxy already
    // replayed this request once (presence of the `fly-replay-src` header).
    let (job, already_replayed) = read_request(&mut conn)?;

    match SLOT.try_lock() {
        // Free: run it.
        Ok(guard) => run_and_reply(&mut conn, &job, guard),

        // Busy, first touch: bounce once to a different instance of this app.
        // Body is a few hashes, well under the 1MB replay cap.
        Err(_) if !already_replayed =>
            reply(&mut conn, 503, "", &[("fly-replay", "elsewhere=true")]),

        // Busy, already bounced once: don't bounce again — wait our turn here.
        Err(_) => {
            let guard = SLOT.lock().map_err(|_| "slot poisoned")?;
            run_and_reply(&mut conn, &job, guard)
        }
    }
}

fn run_and_reply(conn: &mut TcpStream, job: &Job, _slot: MutexGuard<()>)
    -> Result<(), String>
{
    // IDENTICAL to today's container entrypoint call (compute.rs:129-146),
    // just in-process via fork+exec instead of across the docker boundary.
    // std/salt/stack move from container-env to per-request env.
    let out = Command::new("/proc/self/exe")
        .arg("entrypoint")
        .arg(format!("--args={}", job.args))
        .env(caos::STD_ENV, &job.std)            // "CAOS_STD"
        .env("CAOS_SALT", &job.salt)             // use the existing const
        .env(RUN_STACK_ENV, &job.stack)          // "CAOS_RUN_STACK"
        .output()
        .map_err(|e| format!("spawning entrypoint: {e}"))?;

    // Reset the VM BEFORE releasing the slot, so the next job starts clean
    // regardless of the redirect-vs-block path above.
    reset_after_job();

    if out.status.success() {
        reply(conn, 200, &String::from_utf8_lossy(&out.stdout), &[])  // "<type> <hash>"
    } else {
        let tail = String::from_utf8_lossy(&out.stderr);
        reply(conn, 500, tail.trim_end(), &[])
    }
}
```

### Cleanup (the disposable-container guarantees we now make ourselves)

```rust
fn reset_after_job() {
    // 1. Reap strays. The old container teardown killed everything for free;
    //    a reused VM does not. The slot means one job at a time and the worker
    //    uid is shared, so killing every WORKER_UID process is safe and catches
    //    anything the worker backgrounded.
    reap_uid(caos::env_u32(WORKER_UID_ENV).unwrap_or(DEFAULT_WORKER_UID));

    // 2. Wipe the only worker-writable surface besides /cas. `scratch()` writes
    //    under /tmp (worker-common/src/lib.rs:146); /cas is already wiped by
    //    entrypoint, before & after. Add the other classic sticky dirs if the
    //    image ships them.
    for d in ["/tmp", "/var/tmp", "/dev/shm"] { wipe_contents(d); }
}

fn reap_uid(uid: u32) {
    // kill(-1-ish) every process owned by uid. Simplest: iterate /proc, match
    // Uid in /proc/<pid>/status, send SIGKILL. (Or shell out to `pkill -9 -u`.)
}

fn wipe_contents(dir: &str) {
    // Remove children of `dir` but keep the mount point. With /tmp on tmpfs this
    // is near-instant and leaves nothing.
}
```

**Enforcement note:** "only `/tmp` writeable" isn't a new rule we add — it's
already true (worker is unprivileged; `/cas` is root-owned via setuid; scratch is
`/tmp`). To keep it true: in the image build, set `TMPDIR=/tmp`, `HOME=/tmp`,
mount `/tmp` and `/cas` as **tmpfs**, and ensure no other world-writable dirs
ship. Read-only rootfs is optional belt-and-suspenders, not required.

### Minimal HTTP (matches the project's no-framework style)

```rust
// read_request: read request line + headers + Content-Length body off the
// stream by hand (the project already hand-rolls HTTP server-side). Parse the
// JSON body {args,std,salt,stack}. Return (Job, already_replayed) where
// already_replayed = headers.contains_key("fly-replay-src").
//
// reply: write "HTTP/1.1 <code>\r\n" + extra headers + Content-Length + body.
//        fly intercepts the response when it carries `fly-replay`; the client
//        never sees that 503.
```

---

## Piece 2 — dispatcher (server side)

Replace the `docker run` block in `run()` (`compute.rs:129-160`) with a call
through the backend:

```rust
// after cache-miss, cycle check, and resolving the worker version H:
let result = match &config.backend {
    Backend::Docker => dispatch_docker(config, &docker_ref, &args, &std, &salt, &child_stack)?,
    Backend::Fly(fly) => {
        fly::ensure_worker_app(config, fly, &image)?;     // provision once (Piece 3)
        dispatch_http(config, &image, &args, &std, &salt, &child_stack)?
    }
};
// ... rest of run() (cache_set, pin_result) is unchanged; `result` is still
//     the "<type> <hash>" string.
```

```rust
fn dispatch_http(config: &Config, h: &str, args: &str, std: &str, salt: &str,
                 stack: &str) -> Result<String, HttpError>
{
    let url  = format!("http://caos-worker-{h}.flycast/run");   // proxy: LB+autostart+replay
    let body = json!({ "args": args, "std": std, "salt": salt, "stack": stack });
    // POST with bounded retry/backoff: a 503 here means the whole pool is at
    // hard_limit and the proxy queue gave up — retry, then surface as 500.
    let resp = http_post_retry(&url, &body, /*tries*/ 5)?;
    let result = resp.trim().to_string();
    if result_hash(&result).is_empty() {
        return Err(HttpError::new(500, "worker returned no result"));
    }
    Ok(result)
}
```

`dispatch_docker` is today's `Command::new(docker_bin).arg("run")…` lifted into a
function verbatim — the fallback that keeps tilt working.

---

## Piece 3 — provisioner (server side, new `crates/server/src/fly.rs`)

Runs once per worker *version* (small, stable set), gated by a Redis marker so
warm dispatch skips it entirely. Mirrors the existing `caos:image:<h>` cache idea.

```rust
fn ensure_worker_app(config: &Config, fly: &Fly, h: &str) -> Result<(), HttpError> {
    let marker = format!("caos:fly:{h}");
    if let Ok(Some(_)) = cache_get(&config.redis_addr, &marker) {
        return Ok(());                                   // already provisioned
    }

    let app = format!("caos-worker-{h}");

    // 1. Create the app (idempotent). The registry path is per-app, so this
    //    MUST happen before the push.
    fly_api_create_app(fly, &app)?;                      // POST /v1/apps  (org-scoped)

    // 2. Push the converted OCI image to fly's registry, tagged by content hash
    //    so it's immutable + idempotent. This is the EXISTING image-conversion
    //    code (ensure_layer/push_blob/push_manifest), retargeted:
    //      host: caos-registry:5000  ->  registry.fly.io
    //      auth: none                ->  Bearer <deploy token>  (user "x")
    //    Skip if registry.fly.io/<app>:<h> already exists (HEAD the manifest).
    push_image_to_fly_registry(config, fly, &app, h)?;

    // 3. Create N stopped machines from that image, with the service config.
    let image = format!("registry.fly.io/{app}:{h}");
    for _ in 0..fly.machines_per_worker {
        fly_api_create_machine(fly, &app, &image, &worker_service_config())?;
    }

    let _ = cache_set(&config.redis_addr, &marker, "provisioned");
    Ok(())
}
```

**Token handling:** caosd holds one org-scoped **deploy token**
(`CAOS_FLY_TOKEN`), used both as the Machines API bearer and as the
`registry.fly.io` password (user `x`). Do **not** use `fly auth docker` — those
tokens expire after 5 minutes.

**Rate limit:** Machines API create is ~1 req/s (burst 3). Only first-provision
of a new worker version hits it; serialize provisioning behind a small queue if
many new versions can appear at once. Warm dispatch never touches the API.

**GC (separate, optional):** a periodic sweep destroys apps for worker versions
unused for T, clearing their `caos:fly:<h>` marker. App deletes are capped at
100/min — plenty.

---

## Worker machine service config (per machine, set at create time)

```toml
[[services]]
  internal_port = 8080
  protocol      = "tcp"
  auto_start_machines  = true
  auto_stop_machines   = "stop"
  min_machines_running = 0
  [services.concurrency]
    type       = "requests"
    hard_limit = 1          # proxy does the real fan-out + autostart;
    soft_limit = 1          # 1/1 is safe here because serve self-guards
```

caosd machine: a normal always-on app with `min_machines_running >= 1`, no
autostop, and `[mounts]` for the repo Volume at `/git`.

---

## Local testability

`CAOS_BACKEND=docker` keeps everything as-is under tilt. To exercise `serve`
without fly, add a third backend `Backend::LocalServe` that `docker run -d`s one
worker container running `caos serve` and POSTs to it over `caos-net` — same
code path as fly minus the proxy/provisioner, so the slot/redirect/cleanup logic
is testable locally before any fly account is involved.

---

## Measured (local serve pool, tilt, hello worker)

Validated end-to-end under tilt with `CAOS_BACKEND=serve` and a warm `caos serve`
container (cold runs = fresh salt, real worker each time):

| Path | End-to-end | Server-side (cache-miss → ran-worker) |
|---|---|---|
| `docker run` per request (baseline) | ~235 ms | ~180 ms |
| **warm `serve` pool** | **~59 ms** | **~5 ms** |

Server-side worker time dropped ~36× (≈180 → ≈5 ms); end-to-end ~4× (≈176 ms
saved/run), matching the ~65 ms projection. Remaining ~54 ms is CLI-side git push
+ result fetch — runtime-agnostic, now the dominant cost.

**Local-dev caveat (image staleness, not the socket):** the self-spawn failure
was first mis-blamed on the podman socket collision; `/var/run/docker.sock` and
the dev podman are the *same* instance, and caos-server reaches a serve container
on `caos-net` by name (the pre-warm succeeded). The real cause: the server
spawned a **stale, pre-`serve` image** (the converted digest's `/bin/caos` lacked
`serve`), so the container exited and `--rm` removed it. `std/<name>` still
resolved to the old image hash and Redis still cached the old `caos:image:<hash>`
→ old digest. Fix: republish builtins so the publish path tracks the rebuilt
binary, and bust the stale `caos:image:*` Redis keys; then self-spawn works. On
fly this is moot — caosd POSTs to fly-managed worker apps and never spawns
containers.

## Fly — validated live (probe app, then torn down)

The whole worker provision→dispatch chain was exercised against real fly and
codified in `compute.rs` (`Backend::Fly`, `CAOS_BACKEND=fly`):

- **Auth:** the personal `fly auth token` **cannot create apps** (403 on
  `POST /v1/apps` and `GET /v1/apps`); a minted **org deploy token**
  (`fly tokens create org`) works (201). Per-app reads work with either. → the
  provisioner needs an org token (now in `.caos-dev/fly.env`).
- **Validated calls:** `POST /v1/apps {app_name, org_slug}`; push to
  `registry.fly.io/<app>` (`docker login -u x -p <token>`, then push);
  `POST /v1/apps/{app}/machines` with `config.init.exec=["/bin/caos","serve"]`,
  a `services` block (`internal_port:8080`, `443 tls,http`,
  `concurrency{requests,1,1}`, `autostop:stop`, `autostart:true`),
  `guest{shared,1,256}`; `ips allocate-v4 --shared` (free) + v6.
- **End-to-end proof:** a POST to `https://<app>.fly.dev/run` autostarted the
  machine and the `caos serve` worker forked the entrypoint, failing only at the
  intentionally-dead `CAOS_SERVER_URL` (`http 500: connection refused to
  127.0.0.1:1`) — i.e. provision + route + serve + fork all work.

**Transport decisions baked into the code:** Machines API over the internal
plain-HTTP endpoint (`http://_api.internal:4280/v1`, fits TLS-free `minreq`);
image push shelled out to `skopeo` (fly registry is HTTPS + token-auth). On fly,
the worker reaches caosd at `http://caos-server.internal`.

## Fly — full green end-to-end achieved (2026-06-27)

`caos-cli run /cas/std/hello` against caosd on fly returned `receipt: "worker
ran"`, exit 0. The whole stack runs as fly apps in org `personal`, region `sjc`:
`caos-server-mh1` (caosd, Volume `vol_…` for `/git`), `caos-registry-mh1`
(`registry:2`), `caos-redis-mh1` (`redis:7`); workers are dynamically-provisioned
`caos-worker-<hash16>` apps. caosd machine env carries `CAOS_BACKEND=fly`, the org
token (set as a machine `--env`, since `fly secrets deploy` won't target
hand-created machines), and the `*.internal` addresses of the three infra apps.

**Dispatch transport changed from flycast to direct 6PN.** Flycast needs a
private IP allocated via an HTTPS/GraphQL call `minreq` can't make (and `flyctl`
in the image is ~70 MB of bloat). Instead caosd lists the worker app's machines
over the plain-HTTP Machines API, starts a stopped one, and POSTs the job to
`http://<machine_id>.vm.<app>.internal:8080/run` — the machine's internal port
over 6PN, no proxy. A busy worker answers 503 (its SLOT is held) and caosd tries
the next machine, blocking only if all are busy — the "block until available"
half of the approved design. The worker runs jobs identically (forks
`entrypoint` one at a time), so load never changes execution. The `services`
block was dropped from the worker machine config: with direct addressing the
proxy isn't involved, and a proxy `autostop` monitor seeing zero proxy traffic
could otherwise stop a worker mid-job. (Trade-off: no proxy-driven scale-to-zero;
caosd owns machine lifecycle. Scale-down is a follow-up.)

**Five fixes between "validated" and "green" (all in `compute.rs`/`main.rs`):**
1. `caosd` must bind `[::]:80`, not `0.0.0.0:80` — 6PN is IPv6-only, so a worker's
   callback to `caos-server.internal` (an AAAA record) hit nothing on IPv4. This
   was the last-mile bug; `caos serve` already bound `[::]:8080`, which is why
   dispatch *to* the worker worked before the callback did.
2. Address the worker by `<id>.vm.<app>.internal`, not the raw `[ipv6]:port`
   literal — `minreq` mishandles bracketed-IPv6 URLs.
3. `skopeo --insecure-policy` — the slim server image ships no
   `/etc/containers/policy.json`.
4. `skopeo --dest-tls-verify=false` — the slim image ships no CA bundle, so
   skopeo couldn't verify `registry.fly.io`'s cert (the push is still TLS +
   token-auth).
5. `fly_create_app` must treat **422 "Name has already been taken"** (not just
   409) as already-provisioned.

**Warm-path latency:** once the worker app is provisioned (gated by the
`caos:fly:<image>` Redis marker) and the machine is up, dispatch lands on sweep 0
and `ran worker … (cached)` lands in the *same second* — the per-run container
tax is gone, exactly as the warm pool intends.

**Cost note:** worker + 3 infra machines stay running (no autostop on the worker
now). Stop them when idle: `flyctl machine stop <id> -a <app>` per app, or
`flyctl apps destroy caos-worker-<hash16>` for a worker.

## Open questions / TODO

- Confirm the exact replayed-request header name (`fly-replay-src`) against
  current fly docs when wiring `already_replayed`.
- Decide `machines_per_worker` (N) per worker — it caps real concurrency.
- Worker↔server round-trip count is now the dominant latency in distributed
  mode (co-location only shrinks per-RTT cost). Follow-on: batch get/put, or a
  worker-side CAS cache (a persistent sprite would help, but we chose fly for
  multi-machine concurrency).
- Backup policy for the single caosd Volume (daily snapshots + periodic repo
  replication).
