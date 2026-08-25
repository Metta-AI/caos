//! caos-runnerd: the generic runner — the host agent that mints fresh worker
//! containers (see `design/runner-protocol.md`).
//!
//! Each of its slots long-polls the server's `POST /runner/poll` with *no*
//! required args, so it matches any job. On a job it runs
//!
//! ```text
//! docker run --name caos-worker-<nonce> --label caos.runnerd.owner=<id> \
//!     --network <net> -e CAOS_SERVER_URL=<url> [-e CAOS_WORKER_REDIS_ADDR=<a>] \
//!     --entrypoint /bin/caos <image_ref> runner --job=<json>
//! ```
//!
//! and waits for the container to exit, then removes it itself (`docker rm -f`)
//! — NOT `--rm`, which would make the CLI wait on `condition=removed` and pin a
//! core inside podman; see `run_container`. The container owns the job from here:
//! it posts the result itself, then polls for more work for its image (that's
//! what makes it a warm runner) — this slot doesn't poll again until the
//! container dies, so each slot is exactly one machine's worth of capacity.
//! runnerd is only the crash backstop: a container that exits nonzero may have
//! died before posting, so runnerd posts a failure result with the captured
//! log (harmlessly answered 410 if the container already reported).
//!
//! Forcing `--entrypoint /bin/caos` means any image carrying the `caos` binary
//! and a `/worker` works as a compute image, regardless of its own configured
//! entrypoint/command.
//!
//! Configuration (env): `CAOS_SERVER_URL` (default `http://caos-server`; also
//! injected into the containers), `CAOS_RUNNER_TOKEN` (bearer token, if the
//! server requires one), `CAOS_RUNNER_SLOTS` (default 8), `CAOS_DOCKER_NETWORK`
//! (default `caos-net`), `CAOS_DOCKER_BIN` (default `docker`),
//! `CAOS_DOCKER_ARGS` (global flags before `run`, e.g.
//! `--remote --url unix://…` for the socket-delegation backend),
//! `CAOS_RUNNER_SOCKET` (an engine socket, granted only to images that declare
//! `CAOS_GRANT_ENGINE_SOCKET=1` in their own config env; `CAOS_GRANT_VOLUMES`
//! is declared the same way and backs the listed mountpoints with persistent
//! named volumes), and
//! `CAOS_WORKER_REDIS_ADDR` (a redis a worker may use for its own caching,
//! injected into the containers; unset means none is offered).
//!
//! The redis address is the same two-value shape as `CAOS_SERVER_URL`, for the
//! same reason: where workers share the stack's netns it is loopback, and where
//! they get their own netns on a bridge it has to be a name that resolves
//! there. `stack/serve` supplies whichever the placement needs.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The caos binary inside every compute image, forced as the entrypoint.
const CAOS_BIN: &str = "/bin/caos";

/// Fixed in-container path where a granted engine socket is bind-mounted
/// (`CAOS_RUNNER_SOCKET`); advertised to the worker as `CAOS_ENGINE_SOCKET`.
/// Its HOST path rides alongside as `CAOS_ENGINE_SOCKET_HOST`, for a worker
/// that has to mount it into a container of its own — see `run_container`.
const ENGINE_SOCKET_PATH: &str = "/run/caos/engine.sock";

/// An image declares in its own config env that it hosts a caos stack and
/// needs the engine socket (design/test-stack-image.md). Same shape as the
/// `CAOS_WORKER_UID=0` root grant: the image states what containment it needs,
/// and the runner decides whether to honour it. Before this, every worker in
/// the pool got the socket because one image needed it.
const SOCKET_GRANT_ENV: &str = "CAOS_GRANT_ENGINE_SOCKET";

/// Declared the same way and read from the same place: the mountpoints an image
/// wants backed by persistent named volumes. See `ImageGrants::volumes`.
const VOLUME_GRANT_ENV: &str = "CAOS_GRANT_VOLUMES";

/// Declared the same way: this image needs CAP_SYS_ADMIN. The one caller is an
/// image that mounts a persistent store OVER its own — `mount --bind` is the
/// only way to replace a directory whose replacement lives on a different
/// filesystem (rename is EXDEV), and it needs the capability.
const SYS_ADMIN_GRANT_ENV: &str = "CAOS_GRANT_SYS_ADMIN";

/// Declared the same way: device nodes this image needs passed through.
///
/// The one caller wants `/dev/fuse`, because it runs a container engine of its
/// own and that engine's overlay driver is `fuse-overlayfs` here. Without the
/// device it falls back to `vfs`, which copies a whole rootfs per container
/// instead of layering — measured at 1.84s against 0.25s for three starts of a
/// 120 MB image, and the gap grows with image size.
const DEVICE_GRANT_ENV: &str = "CAOS_GRANT_DEVICES";

/// Label stamped on every worker container we start, so a restarted runnerd can
/// find and delete the containers its predecessor left behind. Its value is
/// `owner_id()`, not a constant: containers are not `--rm` any more, so the
/// reaper is what keeps a crash from leaking them.
const OWNER_LABEL: &str = "caos.runnerd.owner";

/// Which runnerd owns a container. The hostname is this runnerd's container id
/// under docker/podman — stable across process restarts inside one container,
/// and distinct from any other runnerd sharing the same engine (an inner
/// runnerd delegating over `--remote` is a different container). Falls back to
/// a literal, which only costs the reaper precision, never correctness: it only
/// ever deletes containers already in `exited`.
fn owner_id() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_string())
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// How long each generic poll hangs. Purely a reconnect cadence — a generic
/// runner never idles out, it just polls again.
const POLL_TTL: Duration = Duration::from_secs(60);

/// Backoff after a failed poll (server down or restarting).
const POLL_RETRY: Duration = Duration::from_secs(2);

struct Config {
    server_url: String,
    /// Address (host:port) of a redis a worker may use for its OWN caching,
    /// injected as `CAOS_WORKER_REDIS_ADDR` into every worker container. This
    /// is a courtesy, not a contract: unset means no worker is offered one, so
    /// a worker must treat its absence as "no cache available" and still do the
    /// work, never as a reason to fail.
    worker_redis_addr: Option<String>,
    token: Option<String>,
    slots: u32,
    network: String,
    docker_bin: String,
    /// Global args inserted *before* the `run` subcommand — e.g.
    /// `--remote --url unix://…` so an INNER runnerd delegates to an outer
    /// engine's API socket (the sibling/socket-delegation backend,
    /// design/cargo-workers.md phase 4) instead of running a nested runtime.
    docker_args: Vec<String>,
    /// The engine socket this runnerd can grant. It is bind-mounted only into
    /// worker containers whose IMAGE asks for it (`SOCKET_GRANT_ENV` in the
    /// image's own config env) — the socket is root-equivalent over this
    /// engine, so it is a per-image grant, declared by the image the same way
    /// `CAOS_WORKER_UID=0` declares its root grant. Unset here means no worker
    /// can be granted it at all, whatever the image claims.
    socket: Option<String>,
    /// Memo for `image_grants`: image ref -> what it declared. Refs are
    /// content-addressed, so a cached answer cannot go stale, and the same
    /// handful of images run over and over.
    grants: Mutex<HashMap<String, ImageGrants>>,
}

/// What an image asks for in its own config env. Both are read from the IMAGE,
/// never from a job or its args, so a caller cannot talk its way into either —
/// only the image's author can, and an image is content-addressed.
#[derive(Clone, Default)]
struct ImageGrants {
    /// `CAOS_GRANT_ENGINE_SOCKET=1`.
    socket: bool,
    /// `CAOS_GRANT_VOLUMES=<abs path> [<abs path>…]` — mountpoints this image
    /// wants backed by persistent named volumes.
    ///
    /// NOT EXCLUSIVE, and that is the whole design rather than an omission.
    /// What lands in them — a nix store, a git object database — is
    /// content-addressed and safe for concurrent writers, so several workers
    /// share one volume rather than queueing for it. The one thing that is NOT
    /// safe that way is redis, which is why a dev stack points at an existing
    /// redis instead of running its own on a shared directory: two servers on
    /// one dbdir both start, neither locks, and they interleave one AOF between
    /// two disagreeing datasets (measured).
    ///
    /// The image names PATHS; runnerd names the volumes. So an image cannot
    /// address another deployment's storage, and nothing about the host's
    /// filesystem layout has to reach a worker.
    volumes: Vec<String>,
    /// `CAOS_GRANT_DEVICES=<abs path> [<abs path>…]` — device nodes to pass
    /// through. Validated like a mountpoint; runnerd decides nothing about what
    /// they are, only that the image asked by absolute traversal-free path.
    devices: Vec<String>,
    /// `CAOS_GRANT_SYS_ADMIN=1` — add CAP_SYS_ADMIN.
    ///
    /// Asked for by exactly one thing: an image that mounts a persistent store
    /// over its own with `mount --bind`. Nothing else can do that job. A
    /// volume and a container's overlay are different filesystems (measured:
    /// device 178 against 37), so `rename` and `renameat2(RENAME_EXCHANGE)` are
    /// both EXDEV, and a symlink swap cannot be atomic — the `ln` that would
    /// make it has its ELF interpreter under the very path being replaced, so
    /// it cannot even start once the old one is gone.
    ///
    /// SMALLER THAN IT SOUNDS, in this company. It is CAP_SYS_ADMIN inside the
    /// container's user namespace, and the image that asks for it already holds
    /// the engine socket, which is root-equivalent on the HOST. This does not
    /// widen what that image can reach; it lets it arrange its own filesystem.
    sys_admin: bool,
}

/// The volume backing `path` for this deployment. Derived, never taken from the
/// image: a mountpoint is a claim about the container's own filesystem, which is
/// safe for an image to make, while a volume NAME is a claim about shared
/// storage, which is not.
fn volume_name(path: &str) -> String {
    let slug: String = path
        .trim_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("caos-vol-{slug}")
}

/// Is this a mountpoint an image may ask for? Absolute, no traversal, no room
/// for a second `-v` argument to hide in.
fn valid_mountpoint(path: &str) -> bool {
    path.starts_with('/')
        && path.len() > 1
        && !path.contains("..")
        && !path.contains(char::is_whitespace)
        && !path.contains(':')
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Install handlers so the process terminates on `SIGINT`/`SIGTERM` — as PID 1
/// in a container the kernel applies no default disposition, so without these
/// `docker stop` would hang until the 10s `SIGKILL`.
fn install_termination_handlers() {
    extern "C" fn terminate(_signum: std::ffi::c_int) {
        unsafe { exit_now(0) }
    }
    extern "C" {
        fn signal(signum: std::ffi::c_int, handler: extern "C" fn(std::ffi::c_int)) -> usize;
        #[link_name = "_exit"]
        fn exit_now(code: std::ffi::c_int) -> !;
    }
    const SIGINT: std::ffi::c_int = 2;
    const SIGTERM: std::ffi::c_int = 15;
    unsafe {
        signal(SIGINT, terminate);
        signal(SIGTERM, terminate);
    }
}

/// Resolve the server URL's host to an ADDRESS, once, and hand that on.
///
/// A name costs about a second per lookup inside a worker's netns — the
/// container resolver is asked for each search-domain permutation, A and AAAA —
/// and git makes roughly ten lookups per operation. Measured from a worker:
/// `git ls-remote http://caos-server` 10022ms, the same by address 50ms, and a
/// single `curl` 1046ms against 41ms. Every worker pays that on every push and
/// every fetch, so it is the largest fixed cost in a test job.
///
/// Resolved HERE because this is the one place it can be paid once: runnerd
/// outlives every worker, and hands the result to all of them. It also fixes
/// runnerd's own long-polling, which would otherwise pay a lookup per poll.
///
/// IPv4 by preference — the AAAA half is what makes these lookups slow, and a
/// v6 literal would need bracketing in the URL. An unresolvable name is left
/// exactly as it was: this is a shortcut, never the thing that decides whether
/// the server is reachable.
fn as_address(url: &str) -> String {
    use std::net::ToSocketAddrs;
    let resolved = (|| {
        let (scheme, rest) = url.split_once("://")?;
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let (host, port_text) = match hostport.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
            _ => (hostport, None),
        };
        if host.parse::<std::net::IpAddr>().is_ok() {
            return None;
        }
        let port: u16 = port_text.and_then(|p| p.parse().ok()).unwrap_or(80);
        let ip = (host, port)
            .to_socket_addrs()
            .ok()?
            .find(|a| a.is_ipv4())?
            .ip();
        Some(match port_text {
            Some(p) => format!("{scheme}://{ip}:{p}{path}"),
            None => format!("{scheme}://{ip}{path}"),
        })
    })();
    match resolved {
        Some(addr) => {
            eprintln!("caos-runnerd: serving workers {url} as {addr}");
            addr
        }
        None => url.to_string(),
    }
}

fn main() {
    install_termination_handlers();
    let config = Arc::new(Config {
        server_url: as_address(&env_or("CAOS_SERVER_URL", "http://caos-server")),
        worker_redis_addr: std::env::var("CAOS_WORKER_REDIS_ADDR")
            .ok()
            .filter(|s| !s.is_empty()),
        token: std::env::var("CAOS_RUNNER_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
        slots: env_or("CAOS_RUNNER_SLOTS", "8").parse().unwrap_or(8),
        network: env_or("CAOS_DOCKER_NETWORK", "caos-net"),
        docker_bin: env_or("CAOS_DOCKER_BIN", "docker"),
        docker_args: std::env::var("CAOS_DOCKER_ARGS")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        grants: Mutex::new(HashMap::new()),
        socket: std::env::var("CAOS_RUNNER_SOCKET")
            .ok()
            .filter(|s| !s.is_empty()),
    });
    eprintln!(
        "caos-runnerd: {} slots, server {}, network {}",
        config.slots, config.server_url, config.network
    );
    reap_leftovers(&config);
    let mut threads = Vec::new();
    for slot in 1..config.slots {
        let config = Arc::clone(&config);
        threads.push(std::thread::spawn(move || slot_loop(&config, slot)));
    }
    slot_loop(&config, 0);
}

/// Does this image ASK for the engine socket (`SOCKET_GRANT_ENV` in its config
/// env)? Read from the image itself, not from the job or its args, so a caller
/// cannot talk its way into the grant — only the image's author can, and the
/// image is content-addressed.
///
/// `image inspect` needs the image present, and `run` is what normally pulls
/// it, so a miss pulls first. Both answers are memoized per ref: a wrong
/// answer here is a silent loss of containment, so the failure mode is chosen
/// deliberately — anything that goes wrong reads as "no grant".
fn image_grants(config: &Config, image_ref: &str) -> ImageGrants {
    if let Some(known) = config.grants.lock().expect("grant memo").get(image_ref) {
        return known.clone();
    }
    let inspect = |args: &[&str]| {
        Command::new(&config.docker_bin)
            .args(&config.docker_args)
            .args(args)
            .output()
    };
    let mut out = inspect(&[
        "image",
        "inspect",
        "--format",
        "{{range .Config.Env}}{{println .}}{{end}}",
        image_ref,
    ]);
    if !matches!(&out, Ok(o) if o.status.success()) {
        // Not pulled yet: `run` would have fetched it, so fetch it here.
        let _ = inspect(&["pull", image_ref]);
        out = inspect(&[
            "image",
            "inspect",
            "--format",
            "{{range .Config.Env}}{{println .}}{{end}}",
            image_ref,
        ]);
    }
    let mut grants = ImageGrants::default();
    match &out {
        Ok(o) if o.status.success() => {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let line = line.trim();
                if line == format!("{SOCKET_GRANT_ENV}=1") {
                    grants.socket = true;
                } else if line == format!("{SYS_ADMIN_GRANT_ENV}=1") {
                    grants.sys_admin = true;
                } else if let Some(list) = line.strip_prefix(&format!("{DEVICE_GRANT_ENV}=")) {
                    for path in list.split_whitespace() {
                        if valid_mountpoint(path) {
                            grants.devices.push(path.to_string());
                        } else {
                            eprintln!("caos-runnerd: {image_ref} asks for device {path:?}, which is not an absolute traversal-free path; ignoring it");
                        }
                    }
                } else if let Some(list) = line.strip_prefix(&format!("{VOLUME_GRANT_ENV}=")) {
                    for path in list.split_whitespace() {
                        if valid_mountpoint(path) {
                            grants.volumes.push(path.to_string());
                        } else {
                            eprintln!(
                                "caos-runnerd: {image_ref} asks for mountpoint {path:?}, \
                                 which is not an absolute traversal-free path; ignoring it"
                            );
                        }
                    }
                }
            }
        }
        // The wrong answer here is a silent loss of containment, so anything
        // that goes wrong reads as "nothing granted".
        _ => eprintln!("caos-runnerd: cannot inspect {image_ref}; granting it nothing"),
    }
    if grants.socket {
        eprintln!("caos-runnerd: {image_ref} declares {SOCKET_GRANT_ENV}; granting engine socket");
    }
    if !grants.volumes.is_empty() {
        eprintln!(
            "caos-runnerd: {image_ref} declares {VOLUME_GRANT_ENV}; granting {}",
            grants.volumes.join(" ")
        );
    }
    config
        .grants
        .lock()
        .expect("grant memo")
        .insert(image_ref.to_string(), grants.clone());
    grants
}

/// One slot: poll for a job, run its container, wait for the container to die,
/// poll again. The container (a warm runner) may serve many jobs before dying;
/// this slot stays parked on `wait` the whole time — one poll per slot lineage.
fn slot_loop(config: &Config, slot: u32) {
    loop {
        match poll(config) {
            Ok(Some(job)) => run_container(config, slot, &job),
            Ok(None) => {} // idle (or evicted, which we ignore): poll again
            Err(e) => {
                eprintln!("runnerd slot {slot}: poll failed: {e}");
                std::thread::sleep(POLL_RETRY);
            }
        }
    }
}

/// A claimed job: the fields runnerd itself needs, plus the payload verbatim to
/// hand the container. (`req` is the wire field name; its value is the ArgTree
/// hash.)
struct Job {
    arg_tree: String,
    nonce: String,
    image_ref: String,
    payload: String,
}

/// One generic long-poll. `Some(job)` to run; `None` on idle/evicted.
fn poll(config: &Config) -> Result<Option<Job>, String> {
    let body = serde_json::json!({
        "required": {},
        "lineage": [],
        "ttl_ms": POLL_TTL.as_millis() as u64,
    });
    let url = format!("{}/runner/poll", config.server_url.trim_end_matches('/'));
    let mut req = minreq::post(&url)
        .with_header("content-type", "application/json")
        .with_header(caos_world::WORLD_HEADER, caos_world::WORLD)
        .with_timeout(POLL_TTL.as_secs() + 15)
        .with_body(body.to_string());
    if let Some(token) = &config.token {
        req = req.with_header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send().map_err(|e| format!("POST {url}: {e}"))?;
    if resp.status_code != 200 {
        return Err(format!(
            "poll failed ({}): {}",
            resp.status_code,
            resp.as_str().unwrap_or("")
        ));
    }
    let v: serde_json::Value = serde_json::from_str(resp.as_str().unwrap_or(""))
        .map_err(|e| format!("invalid poll reply: {e}"))?;
    let Some(job) = v.get("job").filter(|j| j.is_object()) else {
        return Ok(None);
    };
    let field = |k: &str| job[k].as_str().unwrap_or_default().to_string();
    let parsed = Job {
        arg_tree: field("req"),
        nonce: field("nonce"),
        image_ref: field("image_ref"),
        payload: job.to_string(),
    };
    if parsed.arg_tree.is_empty() || parsed.nonce.is_empty() || parsed.image_ref.is_empty() {
        return Err(format!(
            "job missing req/nonce/image_ref: {}",
            parsed.payload
        ));
    }
    Ok(Some(parsed))
}

/// Run the job's container and wait it out. The container posts its own
/// results; we only backstop a crash — nonzero exit means it may never have
/// reported, so post a failure with the captured log (410 if it did report).
///
/// Deliberately NOT `--rm`: an *attached* `docker run --rm` waits with
/// `condition=removed`, i.e. `POST /containers/<id>/wait?condition=removed`,
/// and podman's compat handler spins a core on that call for as long as the
/// container is stopped-but-not-yet-removed. Podman also loses auto-removals
/// outright (containers sit in `Exited` with `AutoRemove=true` forever), which
/// turns that spin into a permanent one. So we take exit-wait only — which the
/// engine answers from an event, not a poll — and delete the container
/// ourselves afterwards.
fn run_container(config: &Config, slot: u32, job: &Job) {
    eprintln!(
        "runnerd slot {slot}: arg_tree {} -> container ({})",
        job.arg_tree, job.image_ref
    );
    let name = format!("caos-worker-{}", job.nonce);
    let mut command = Command::new(&config.docker_bin);
    command
        .args(&config.docker_args) // global flags (e.g. --remote --url …) precede `run`
        .arg("run")
        .args(["--name", &name])
        // Our mark for the crash reaper — see `reap_leftovers`. Scoped to this
        // runnerd, because an INNER runnerd delegating to the outer engine
        // (`--remote --url …`) shares a container namespace with the outer one,
        // and must not sweep its siblings' containers.
        .args(["--label", &format!("{OWNER_LABEL}={}", owner_id())])
        .args(["--network", &config.network]);
    // One inspect answers both grants, memoized together.
    let grants = image_grants(config, &job.image_ref);
    if let Some(sock) = &config.socket {
        if grants.socket {
            // Hand THIS worker the engine socket so its own inner runnerd can
            // delegate sibling containers to this engine (phase 4). Bind it at
            // a fixed in-container path and advertise that path, so a worker
            // that only TALKS to the engine never needs to know the host
            // socket's location.
            //
            // …and the host location too, for the one worker that cannot use
            // the fixed path: a bind mount's source is resolved on the HOST, so
            // a worker CREATING a container that itself needs the engine can
            // only name the socket as the host sees it. That is how a shared
            // test stack gets started as a detached sibling
            // (design/faster-tests.md).
            //
            // Not a second grant, and not a wider one: it sits inside the same
            // per-image gate, and a holder of this socket can already read the
            // host path straight out of the engine by inspecting itself. This
            // hands it over rather than making it do the archaeology.
            command
                .args(["-v", &format!("{sock}:{ENGINE_SOCKET_PATH}")])
                .args(["-e", &format!("CAOS_ENGINE_SOCKET={ENGINE_SOCKET_PATH}")])
                .args(["-e", &format!("CAOS_ENGINE_SOCKET_HOST={sock}")]);
        }
    }
    // Persistent storage the image asked for. Named volumes rather than bind
    // mounts, deliberately: the engine owns the name, so nothing about the
    // host's filesystem has to be known by — or reach — a worker. It also seeds
    // itself, because mounting a FRESH named volume over a path that has content
    // in the image copies that content in (verified), which is what lets a nix
    // store survive here without any bootstrap step.
    if grants.sys_admin {
        command.args(["--cap-add", "SYS_ADMIN"]);
    }
    for dev in &grants.devices {
        command.args(["--device", dev]);
    }
    for path in &grants.volumes {
        command.args(["-v", &format!("{}:{path}", volume_name(path))]);
    }
    // A worker's own scratch cache, when this deployment offers one. Passed
    // only when set, so an unset address reaches the worker as an ABSENT env
    // var rather than an empty one it might dial.
    if let Some(addr) = &config.worker_redis_addr {
        command.args(["-e", &format!("CAOS_WORKER_REDIS_ADDR={addr}")]);
    }
    let out = command
        .args(["-e", &format!("CAOS_SERVER_URL={}", config.server_url)])
        .args(["--entrypoint", CAOS_BIN])
        .arg(&job.image_ref)
        .arg("runner")
        .arg(format!("--job={}", job.payload))
        .output();
    // Delete it ourselves, now that we hold its exit status and its log. A
    // plain `rm -f` is one DELETE the engine answers synchronously — none of
    // the auto-remove machinery, and nothing polls.
    remove_container(config, &name);
    let failure = match out {
        Ok(out) => {
            // Relay the container's log (the runner relays its workers' output
            // to its stderr) so it survives the container's removal.
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
            if out.status.success() {
                None
            } else {
                Some((
                    format!("worker container exited with {}", out.status),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                ))
            }
        }
        Err(e) => Some((format!("running {}: {e}", config.docker_bin), String::new())),
    };
    if let Some((error, log)) = failure {
        eprintln!("runnerd slot {slot}: arg_tree {}: {error}", job.arg_tree);
        if let Err(e) = post_failure(config, job, &error, &log) {
            eprintln!("runnerd slot {slot}: reporting failure: {e}");
        }
    }
}

/// Delete one worker container. Best-effort and quiet: a container that is
/// already gone is the normal case for the reaper, and a removal that fails is
/// a leaked container, not a failed job — the job's result is already settled
/// by the time we get here.
fn remove_container(config: &Config, name_or_id: &str) {
    let out = Command::new(&config.docker_bin)
        .args(&config.docker_args)
        .args(["rm", "-f", name_or_id])
        .output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => eprintln!(
            "caos-runnerd: removing {name_or_id}: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => eprintln!("caos-runnerd: removing {name_or_id}: {e}"),
    }
}

/// Delete the exited worker containers this runnerd's predecessor left behind.
/// Without `--rm` the engine no longer cleans up after a runnerd that died
/// mid-run, so this is the replacement — run once at startup, before any slot
/// claims a job.
///
/// `status=exited` is load-bearing as well as safe: our own live workers are
/// still running, and so are any siblings that happen to share the label.
fn reap_leftovers(config: &Config) {
    let filter = format!("label={OWNER_LABEL}={}", owner_id());
    let out = Command::new(&config.docker_bin)
        .args(&config.docker_args)
        .args([
            "ps",
            "-aq",
            "--filter",
            "status=exited",
            "--filter",
            &filter,
        ])
        .output();
    let listed = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            eprintln!(
                "caos-runnerd: listing leftover containers: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return;
        }
        Err(e) => {
            eprintln!("caos-runnerd: listing leftover containers: {e}");
            return;
        }
    };
    let ids: Vec<&str> = listed.split_whitespace().collect();
    if ids.is_empty() {
        return;
    }
    eprintln!("caos-runnerd: reaping {} leftover container(s)", ids.len());
    for id in ids {
        remove_container(config, id);
    }
}

/// The crash backstop: report a container that died without (necessarily)
/// posting. A 410 means it did post before dying — the job is settled.
fn post_failure(config: &Config, job: &Job, error: &str, log: &str) -> Result<(), String> {
    // Keep only the tail of a big log: the failure is usually at the end, and
    // the message lands in an error string a client will read.
    let tail: String = log
        .lines()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let body = serde_json::json!({
        "req": job.arg_tree, "nonce": job.nonce, "ok": false, "error": error, "log": tail,
    });
    let url = format!("{}/runner/result", config.server_url.trim_end_matches('/'));
    let mut req = minreq::post(&url)
        .with_header("content-type", "application/json")
        .with_header(caos_world::WORLD_HEADER, caos_world::WORLD)
        .with_timeout(30)
        .with_body(body.to_string());
    if let Some(token) = &config.token {
        req = req.with_header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send().map_err(|e| format!("POST {url}: {e}"))?;
    match resp.status_code {
        200 | 410 => Ok(()),
        code => Err(format!(
            "result post failed ({code}): {}",
            resp.as_str().unwrap_or("")
        )),
    }
}
