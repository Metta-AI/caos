//! caos server: storage and compute behind one endpoint.
//!
//! Storage (a tiny HTTP front-end over a git object database mounted at `/git`):
//!
//! * `GET  /object/<hash>` — return the serialized object (`<type> <size>\0…`).
//! * `HEAD /object/<hash>` — 200/404, no body: is it stored? (`put` prunes on it.)
//! * `POST /object/` — store the serialized object in the body, return its hash.
//!
//! Compute:
//!
//! * `GET /run?req=<hash>` — run the ArgTree `<hash>` (`req` is the
//!   query param's historical name; its value is the ArgTree hash) and return
//!   the hash of its result.
//! * `POST /start` (a bundled request, `POST /submit`'s body) and `POST
//!   /start/<verb>` (a `.caos-expr` line) — start a request and return its
//!   TICKET at once. Forming the request (a base's build, a locator's fetch)
//!   and running it happen on the ticket's own thread, recorded under it, so
//!   `GET /status/<ticket>` shows the evaluation as well as the run.
//!   `GET /result/<ticket>` says how it stands and `GET /wait/<ticket>` blocks
//!   for the answer. `/eval/<verb>` is a start followed by that wait.
//! * `POST /trace/child` — record, under an in-flight job, that it started work
//!   on another stack, so a reader descends into it (`status`).
//! * `POST /sub-run` — admit an exact detached child under an in-flight job's
//!   existing server-side run stack and secret store.
//! * `GET /eval?root:<type>=<value>[&path=<p>]` — walk `.caos-expr` from a root
//!   down to a path and return what falls out as `"<type> <hash>"`. The root is
//!   a TYPED argument, named by the caller as everything else in caos is:
//!   `root:hash=<oid>` starts from an object this store holds, `root:@@=<locator>`
//!   from a pin, fetching the commit if the store lacks it (see [`mod locator`]).
//!   A client and a worker both ask here rather than following a pin themselves,
//!   which is what lets a server-side evaluation reach one at all.
//!
//! The server runs no workers itself. Dispatch is pull-based (see
//! `design/runner-protocol.md`): runners long-poll `POST /runner/poll` with
//! their required args, the server matches pending `/run` jobs against the
//! parked polls, and the runner posts the job's `"<type> <hash>"` back via
//! `POST /runner/result` (or a `promise` the compute half resolves — see
//! `design/map-then.md`). The generic runner minting fresh worker containers is
//! `caos-runnerd`, an ordinary poller with no required args.
//!
//! Compute results are cached in Redis (`CAOS_REDIS_ADDR`, default
//! `caos-redis:6379`): the key is the arg-tree hash — the ArgTree itself, which
//! carries the worker image, std and salt — the value the
//! result hash. A hit skips the worker entirely. Redis is best-effort — if
//! it's unreachable we log and run uncached.
//!
//! Git transport:
//!
//! * `GET  /info/refs?service=…`, `POST /git-upload-pack`, `POST /git-receive-pack`
//!   — git smart-HTTP over the same repo, so the caos client can use the server as
//!   a `caos` git remote (push objects up, fetch refs/results down). See
//!   [`mod git`]; it delegates to `git http-backend`.
//!
//! The halves live in [`mod storage`], [`mod compute`], [`mod runner`], and
//! [`mod git`]; this file is the entry point, the shared [`Config`]/[`HttpError`],
//! and the request router.

mod compute;
mod git;
mod grant_history;
mod import;
mod locator;
mod loose;
mod mcp;
mod push;
mod remote_git;
mod repair;
mod runner;
mod secret_store;
mod secrets;
mod status;
mod storage;

use std::sync::Arc;
use std::time::Duration;

use tiny_http::{Method, Request, Response, Server, StatusCode};

/// Listen address; overridable for local runs outside the container. Binds the
/// IPv6 wildcard (dual-stack: also accepts IPv4) so runners can reach us over
/// IPv6-only networks too.
const DEFAULT_ADDR: &str = "[::]:80";

/// Where the git object database lives (the storage half, now in-process).
/// Override with `CAOS_GIT_DIR` (useful for local runs outside the container).
const DEFAULT_GIT_DIR: &str = "/git";

/// Registry base URL converted git-docker images are pushed to, reachable from
/// *this* container over the docker network. Override with
/// `CAOS_REGISTRY_PUSH_URL`.
const DEFAULT_REGISTRY_PUSH_URL: &str = "http://caos-registry:5000";

/// How the docker daemon that actually runs workers (runnerd's) refers to that
/// same registry — a published port on localhost, which docker treats as an
/// insecure registry, so no TLS/daemon config is needed. Override with
/// `CAOS_REGISTRY_PULL_HOST`.
const DEFAULT_REGISTRY_PULL_HOST: &str = "localhost:5000";

/// Redis (host:port) used to cache results. Override with `CAOS_REDIS_ADDR`.
const DEFAULT_REDIS_ADDR: &str = "caos-redis:6379";

/// Runtime configuration, read once from the environment at startup. Cloning
/// is cheap and lets admitted sub-runs outlive the request thread that launched
/// them while sharing the same repository handle.
#[derive(Clone)]
struct Config {
    registry_push_url: String,
    registry_pull_host: String,
    redis_addr: String,
    /// What this stack's cache keys are prefixed with, so several stacks can
    /// share one redis without answering each other's questions.
    ///
    /// THE ARGTREE IS NOT ENOUGH ON ITS OWN, and that is the point. It captures
    /// the image, std, salt and every arg — everything the WORK depends on —
    /// but not the caos that ran it. Where a server's behavior changes without
    /// any of those moving, a stale entry is served as a hit: CLAUDE.md records
    /// the observed case, a rebuilt `caos` binary leaving a seeded sentinel's
    /// key unmoved so redis hands back the previous deploy's worker image. Any
    /// value that identifies the build closes that, and folding it into the KEY
    /// beats invalidating entries — nothing has to be deleted, so nothing has to
    /// be deleted correctly.
    ///
    /// It is also what lets two stacks built from different trees run at once
    /// against one redis: their namespaces differ because their builds do, with
    /// no coordination and nothing host-specific in a key.
    ///
    /// Empty (the default) means no prefix at all, so a deployment that sets
    /// nothing keeps exactly the keys it has today.
    cache_namespace: String,
    /// Filesystem path to the git object database, passed to `git http-backend`
    /// as `GIT_PROJECT_ROOT` for the smart-HTTP transport (see [`mod git`]).
    git_dir: String,
    /// `secrets.git`, which nothing else in this server links to.
    secrets_git: String,
    /// The git object database, served directly (storage is now in-process).
    /// Thread-safe: each request thread takes a local handle via `to_thread_local`.
    repo: gix::ThreadSafeRepository,
}

/// Install handlers so the process terminates on `SIGINT`/`SIGTERM`. This matters
/// in a container, where the daemon is PID 1: the kernel applies no default
/// disposition for these signals to PID 1, so without an explicit handler
/// `docker stop` (and `caosd down`'s Ctrl-C) would hang until the 10s `SIGKILL`.
fn install_termination_handlers() {
    // Async-signal-safe: we hold no state that needs flushing, so just exit.
    extern "C" fn terminate(_signum: std::ffi::c_int) {
        unsafe { exit_now(0) }
    }
    extern "C" {
        // libc, resolved against what std already links.
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

fn main() {
    // Run as the repository's `pre-receive` hook (see `loose::publish_quarantine`).
    if loose::is_hook_invocation() {
        std::process::exit(loose::publish_quarantine());
    }
    install_termination_handlers();

    let addr = std::env::var("SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let git_dir = env_or("CAOS_GIT_DIR", DEFAULT_GIT_DIR);

    // This server's git settings live in ITS OWN ENVIRONMENT, never in the
    // repository's config (see `git_settings`). Several servers, of different
    // builds, share one repository (every test stack shares its host stack's),
    // so a setting written into the repo would apply to all of them. The one
    // exception is a repository this process creates (e.g. a fresh fly
    // Volume): nobody else uses it yet, and the protective settings written
    // into it also protect a person who runs git there by hand.
    if gix::open(&git_dir).is_err() {
        initialize_repo(&git_dir).unwrap_or_else(|error| {
            eprintln!("fatal: {error}");
            std::process::exit(1);
        });
    }
    let hooks = std::env::temp_dir().join(format!("caos-server-hooks-{}", std::process::id()));
    let hooks = loose::install_hooks(&hooks).unwrap_or_else(|error| {
        eprintln!("fatal: installing this server's git hooks: {error}");
        std::process::exit(1);
    });
    // Still single-threaded and with no child yet, so every git this process
    // runs against the store sees the settings from the start.
    let _ = GIT_ENV.set(settings_env(&git_settings(Some(&hooks))));

    // Clear what a previous crash left behind BEFORE opening the repo: gix
    // answers "is this object stored?" for a loose object with a path-exists
    // check, so an empty file has to be gone from disk before anything asks.
    let swept = repair::sweep_empty_loose_objects(&git_dir);
    let temporaries = loose::sweep_temporaries(&std::path::Path::new(&git_dir).join("objects"));
    if temporaries > 0 {
        eprintln!(
            "repair: {git_dir} had {temporaries} temporary object file(s) from a crash; removed"
        );
    }

    // Open the object database once as a thread-safe handle; each request thread
    // takes a cheap local handle from it (see `handle`).
    // Nothing writes packs any more (every transfer unpacks; see `mod loose`),
    // but a store from before that, or a seeded one, can still hold them. gix
    // fixes its slot capacity at open time; its client default (at least 32)
    // cannot accommodate many packs, so reserve its supported maximum. A store
    // that is still packed is migrated offline by exploding its packs into
    // loose objects (design/loose-object-store.md), never by a live repack:
    // see the shared-reader constraints above.
    // Given skips gix's initial scan, so diagnose an already-full store here.
    let packs = std::fs::read_dir(std::path::Path::new(&git_dir).join("objects/pack"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "idx"))
                .count()
        })
        .unwrap_or(0);
    if packs >= (1 << 15) - 1 {
        eprintln!("fatal: object store has {packs} packs; explode them into loose objects offline, preserving unreferenced CAS objects, before restarting (design/loose-object-store.md)");
        std::process::exit(1);
    }
    if packs > 0 {
        eprintln!("warning: object store has {packs} pack(s); nothing should write packs any more, so these predate the loose-object store; explode them offline (design/loose-object-store.md)");
    }
    let options = gix::open::Options::default()
        .object_store_slots(gix::odb::store::init::Slots::Given((1 << 15) - 1));
    let repo = match gix::open_opts(&git_dir, options) {
        Ok(repo) => repo.into_sync(),
        Err(err) => {
            eprintln!("fatal: cannot open git repo at {git_dir}: {err}");
            std::process::exit(1);
        }
    };

    // Then the refs the sweep just orphaned, plus any that were already dangling.
    let dropped = repair::drop_broken_refs(&repo.to_thread_local(), &git_dir);
    if swept + dropped > 0 {
        eprintln!(
            "repair: {git_dir} had crash damage — dropped {swept} empty object(s) \
             and {dropped} broken ref(s)"
        );
    }

    // The check below reads every object in the store, which on a long-lived
    // repo takes minutes: far too long to pay on every start, and a test stack
    // has nothing to check. `caosd` opts in (CAOS_STACK_STORE_CHECK); no other
    // placement does. There is no repack here any more: the store is never
    // packed (design/loose-object-store.md).
    if env_or("CAOS_STORE_CHECK", "no") == "yes" {
        // Older servers accepted individual commits without their
        // dependencies. Check all objects, including unreferenced ones, before
        // trusting existence as proof of closure. Never delete history to make
        // an upgrade pass.
        verify_object_closure(&git_dir).unwrap_or_else(|error| {
            eprintln!("fatal: object store has incomplete history; restore the missing objects before restarting: {error}");
            std::process::exit(1);
        });
    }

    let secrets_git = env_or("CAOS_SECRETS_GIT_DIR", &secret_store::default_dir(&git_dir));
    secret_store::init(&secrets_git).unwrap_or_else(|error| {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    });

    // Shared read-only across handler threads (one per request, see below).
    let config = Arc::new(Config {
        registry_push_url: env_or("CAOS_REGISTRY_PUSH_URL", DEFAULT_REGISTRY_PUSH_URL),
        registry_pull_host: env_or("CAOS_REGISTRY_PULL_HOST", DEFAULT_REGISTRY_PULL_HOST),
        redis_addr: env_or("CAOS_REDIS_ADDR", DEFAULT_REDIS_ADDR),
        cache_namespace: env_or("CAOS_CACHE_NAMESPACE", ""),
        git_dir,
        secrets_git,
        repo,
    });

    let server = match Server::http(addr.as_str()) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("fatal: cannot bind {addr}: {err}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "caos-server listening on http://{addr} (storage + compute), \
         git repo {}, registry push {} / pull {}, redis {}",
        config.git_dir, config.registry_push_url, config.registry_pull_host, config.redis_addr,
    );

    spawn_request_ref_pruner(config.git_dir.clone());

    // One thread per request, not a serial loop: a worker fetches its inputs
    // from `/object` while its own `/run` request is still being served, a
    // runner's poll parks for its whole TTL, and several top-level runs may be
    // in flight at once. Threads are cheap here: each mostly blocks (compute
    // fans out its own threads for parallel promise maps — see
    // compute::resolve_promise).
    for request in server.incoming_requests() {
        let config = Arc::clone(&config);
        std::thread::Builder::new()
            .stack_size(compute::EVAL_STACK_BYTES)
            .spawn(move || {
                if let Err(err) = handle(config, request) {
                    // Only reachable if writing the response itself fails.
                    eprintln!("failed to send response: {err}");
                }
            })
            .expect("spawning a request thread");
    }
}

/// How long a required Git command waits out another process's lock, and how
/// often it retries — 5s in 50ms steps.
///
/// A SERVER REPO CAN HAVE A SECOND SERVER, and the settings above are asserted
/// on EVERY boot rather than only on a repo this process created, so two
/// servers coming up against one repo assert the same values at the same
/// instant. `git config` publishes an edit by taking `config.lock`, so the
/// loser gets `could not lock config file config: File exists` and — these
/// being *required* commands — died with `fatal:` before it ever listened. That
/// is the dev placement's ordinary case: several `dev/stack-up` stacks share
/// `/caos-dev/git` deliberately (their objects are content-addressed), and it
/// cost one of two concurrent stacks its whole bring-up, reported as "server
/// never came up" with the real message a file away.
///
/// Waited out rather than tolerated: the assertion still has to happen, and
/// what the loser is waiting for is a peer writing the very same value. Bounded
/// because a lock held longer than this is a STALE one — a crashed writer's
/// leftover `config.lock`, which no amount of patience clears and which a
/// person has to see.
const GIT_LOCK_WAIT_STEPS: u32 = 100;
const GIT_LOCK_WAIT_STEP: Duration = Duration::from_millis(50);

/// Does this failure say another process holds the lock on what we are editing?
///
/// git's lockfile layer reports it the same way for every ref, index and config
/// edit: it tried to create `<thing>.lock` and `File exists`. Matched on the
/// message because that is all `git config` gives us — its exit status for a
/// lock collision is the generic 255.
fn is_git_lock_contention(detail: &str) -> bool {
    detail.contains("File exists")
        && (detail.contains("could not lock") || detail.contains("Unable to create"))
}

/// Run one of the Git commands that establishes the server's storage
/// contract. Ignoring one of these failures lets the server start in a mode
/// where pushes, object fetches, or crash recovery are silently unsafe.
fn run_required_git(args: &[&str]) -> Result<(), String> {
    let command = format!(
        "git {}",
        args.iter()
            .map(|arg| format!("{arg:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let mut last = String::new();
    for attempt in 0..GIT_LOCK_WAIT_STEPS {
        let output = std::process::Command::new("git")
            .args(args)
            .output()
            .map_err(|error| format!("cannot run required command {command}: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        last = if detail.is_empty() {
            format!("required command {command} exited with {}", output.status)
        } else {
            format!(
                "required command {command} exited with {}: {detail}",
                output.status
            )
        };
        if !is_git_lock_contention(detail) {
            return Err(last);
        }
        if attempt == 0 {
            eprintln!("waiting for another process's git lock: {last}");
        }
        std::thread::sleep(GIT_LOCK_WAIT_STEP);
    }
    Err(format!(
        "{last} (still held after {}s — a crashed writer's stale lock file?)",
        GIT_LOCK_WAIT_STEPS as u64 * GIT_LOCK_WAIT_STEP.as_millis() as u64 / 1000
    ))
}

fn verify_object_closure(git_dir: &str) -> Result<(), String> {
    match std::fs::read(std::path::Path::new(git_dir).join("shallow")) {
        Ok(bytes) if !bytes.is_empty() => {
            return Err("server repository must not be shallow".into())
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    use std::process::{Command, Stdio};
    let git = || {
        let mut command = Command::new("git");
        command
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_GRAFT_FILE", "/dev/null")
            .args(["-c", "core.commitGraph=false", "-C", git_dir]);
        command
    };
    // fsck --connectivity-only starts from refs and misses orphan commits.
    // Make every stored object a root, without reading every blob's payload.
    let mut objects = git()
        .args([
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("enumerating stored objects: {error}"))?;
    let walk = git()
        .args([
            "rev-list",
            "--objects",
            "--quiet",
            "--missing=error",
            "--stdin",
        ])
        .stdin(objects.stdout.take().expect("piped object list"))
        .output();
    // Check both processes: a failed enumeration must not look like an empty
    // store, and a failed walk must not be hidden by a successful enumeration.
    let listed = objects
        .wait_with_output()
        .map_err(|error| format!("enumerating stored objects: {error}"))?;
    let walked = walk.map_err(|error| format!("checking object closure: {error}"))?;
    if !walked.status.success() || !listed.status.success() {
        return Err(format!(
            "{}{}",
            String::from_utf8_lossy(&listed.stderr),
            String::from_utf8_lossy(&walked.stderr)
        ));
    }
    Ok(())
}

/// `*.unpackLimit` meaning "always unpack": git keeps a transfer as a pack only
/// when it carries at least this many objects, and this is the largest value it
/// accepts.
const UNPACK_ALWAYS: &str = "2147483647";

/// The git settings this server runs with, as `(key, value)` pairs.
///
/// They reach git through this process's environment (`settings_env`, applied
/// with `git_env` to every git this server runs against the store, and from
/// `http-backend` to receive-pack, upload-pack and the hook), NEVER through the
/// repository's config: several servers of different builds share one
/// repository, and a setting written there would apply to all of them. Only a
/// repository this process creates gets them written in (`initialize_repo`,
/// with `hooks` = `None`). Environment settings outrank the repo's own.
///
/// `hooks` is this process's own hooks directory (`loose::install_hooks`).
fn git_settings(hooks: Option<&std::path::Path>) -> Vec<(&'static str, String)> {
    let mut settings = vec![
        // The server's protocol requirements. A repo can arrive from anywhere
        // (a test stack seeds one by `git init --bare` plus a fetch, and a
        // plain `init` sets none of these), and a missing `http.receivepack`
        // reads as every push coming back `403`, an auth problem in disguise.
        ("http.receivepack", "true"),
        ("uploadpack.allowAnySHA1InWant", "true"),
        // Serve partial-clone (`--filter`) fetches: the std/merge worker
        // fetches the commit GRAPH only (`--filter=tree:0`) and lazily faults
        // in just the three trees `git merge-tree` needs.
        ("uploadpack.allowFilter", "true"),
        // Request refs are transient negotiation anchors, result refs a
        // server-written durability index, and a locator's pin-to-tree memo
        // one ref per pin. None belongs in fetch advertisements. All stay
        // visible to receive-pack negotiation: a client can form its next
        // request from a result object that exists only on the server, so
        // hiding that result makes send-pack try (and fail) to read it.
        ("uploadpack.hideRefs", "refs/caos/req/"),
        ("uploadpack.hideRefs", locator::TREE_REF_PREFIX),
        ("uploadpack.hideRefs", "refs/caos/res/"),
        // Validate every incoming object in git's quarantine, including
        // objects outside the updated refs.
        ("receive.fsckObjects", "true"),
        ("receive.shallowUpdate", "false"),
        // The store is loose objects only and is never packed
        // (design/loose-object-store.md, `mod loose`). Every transfer unpacks
        // into loose objects, however small or large; a push lands in git's
        // quarantine and the hook publishes it children first. Objects are
        // written at zlib level 0: the filesystem compresses.
        ("receive.unpackLimit", UNPACK_ALWAYS),
        ("transfer.unpackLimit", UNPACK_ALWAYS),
        ("fetch.unpackLimit", UNPACK_ALWAYS),
        ("core.looseCompression", "0"),
        // Never let git rewrite this repo's object store behind our back.
        // receive-pack forks a background repack after a push; that rewrites
        // the store while a concurrent upload-pack streams from it (a client
        // sees `fetch-pack: invalid index-pack output`), and the gix handle
        // this server holds asserts when packs move under it, which tiny_http
        // turns into a body-less 500. THREE settings: `gc.auto 0` gates only
        // the `gc` task, and git 2.54's `geometric-repack` maintenance task
        // ignores every gc knob. A cruft repack is data loss, not just a
        // crash: nearly every object here is unreachable from a ref, so it
        // would drop whatever has not been touched in two weeks while redis
        // keeps handing out hashes that point at it.
        ("gc.auto", "0"),
        ("receive.autogc", "false"),
        ("maintenance.geometric-repack.enabled", "false"),
        // Survive an unclean shutdown. git's default publishes a loose object
        // or a ref by renaming a temp file into place without fsyncing, so a
        // crash can leave the right name holding zero bytes, a POISONED object
        // (see [`mod repair`]). `batch` flushes once per batch of writes.
        ("core.fsync", "objects,reference"),
        ("core.fsyncMethod", "batch"),
        // Keep previous ref values as the recovery path `repair` restores
        // damaged refs from. Nothing expires them yet: until a retention
        // policy exists, keep the history rather than invent a window.
        ("core.logAllRefUpdates", "always"),
    ]
    .into_iter()
    .map(|(key, value)| (key, value.to_string()))
    .collect::<Vec<_>>();
    if let Some(hooks) = hooks {
        settings.push(("core.hooksPath", hooks.display().to_string()));
    }
    settings
}

/// `settings` as git's environment configuration (`GIT_CONFIG_COUNT` and its
/// numbered keys and values).
fn settings_env(settings: &[(&str, String)]) -> Vec<(String, String)> {
    let mut env = vec![("GIT_CONFIG_COUNT".to_string(), settings.len().to_string())];
    for (index, (key, value)) in settings.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{index}"), key.to_string()));
        env.push((format!("GIT_CONFIG_VALUE_{index}"), value.clone()));
    }
    env
}

/// This server's git settings, set once at startup (`main`).
static GIT_ENV: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

/// The environment that carries this server's git settings: apply it
/// (`Command::envs`) to every git command run against the store.
pub(crate) fn git_env() -> impl Iterator<Item = (&'static str, &'static str)> {
    GIT_ENV
        .get()
        .into_iter()
        .flatten()
        .map(|(key, value)| (key.as_str(), value.as_str()))
}

/// Create the store's repository, with the settings written into its config:
/// this process is its only user so far, and the written settings also protect
/// a person who later runs git in it by hand. The config is then left
/// read-only as a marker that nothing should change it; git rewrites a config
/// by lock and rename whatever its mode, so the guarantee is that this server
/// never writes it again.
fn initialize_repo(git_dir: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    run_required_git(&["init", "-q", "--bare", git_dir])?;
    for (key, value) in git_settings(None) {
        run_required_git(&["-C", git_dir, "config", "--add", key, &value])?;
    }
    let config = std::path::Path::new(git_dir).join("config");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o444))
        .map_err(|error| format!("making {} read-only: {error}", config.display()))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// An error that maps cleanly onto an HTTP status code + body.
pub(crate) struct HttpError {
    status: u16,
    message: String,
}

impl HttpError {
    pub(crate) fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// Plain-data accessors, for callers that clone an error across threads
    /// (single-flight broadcasts one outcome to every waiter).
    pub(crate) fn status(&self) -> u16 {
        self.status
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl From<std::io::Error> for HttpError {
    fn from(err: std::io::Error) -> Self {
        HttpError::new(500, format!("io error: {err}"))
    }
}

/// Dispatch a single request and send its response.
fn handle(config: Arc<Config>, mut request: Request) -> std::io::Result<()> {
    // Git smart-HTTP (the `caos` remote) is served by a separate CGI delegate that
    // sets its own status/headers, so it bypasses the `route` -> `from_data` path.
    let path = request.url().split('?').next().unwrap_or("").to_string();
    if git::is_git_path(&path) {
        return git::serve(&config, request);
    }
    // MCP over HTTP (design: the caos tool server lives here, not in each
    // harness's client). It sets its own status and content type, so like
    // git it bypasses the `route` -> `from_data` path.
    if path == "/mcp" {
        return serve_mcp(&config, request);
    }
    // The WORLD guard (design/test-stack-image.md): a caos client built for
    // the other world must not drive this stack. The dangerous crossing is
    // silent — a host client against the test stack passes until the tree
    // under test changes the client, and then the suite is exercising host
    // code where the tested code was the point. Requests without the header
    // pass: git smart-HTTP returned above, and health probes are plain curl.
    if let Some(client) = request
        .headers()
        .iter()
        .find(|header| header.field.equiv(caos_world::WORLD_HEADER))
        .map(|header| header.value.as_str().to_string())
    {
        if client != caos_world::WORLD {
            return request.respond(
                Response::from_string(caos_world::mismatch(caos_world::WORLD, &client) + "\n")
                    .with_status_code(StatusCode(400)),
            );
        }
    }
    match route(&config, &mut request) {
        Ok(body) => request.respond(Response::from_data(body)),
        Err(err) => request.respond(
            Response::from_string(format!("{}\n", err.message))
                .with_status_code(tiny_http::StatusCode(err.status)),
        ),
    }
}

/// `POST /mcp`: one JSON-RPC message in, its answer out (`202` with no body for
/// a notification). What the caller is — which step, which secret readers — is
/// in headers, because the endpoint keeps no connection state.
fn serve_mcp(config: &Arc<Config>, mut request: Request) -> std::io::Result<()> {
    let header = |request: &Request, name: &'static str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().to_string())
    };
    if *request.method() != Method::Post {
        return request.respond(
            Response::from_string("POST a JSON-RPC message\n").with_status_code(StatusCode(405)),
        );
    }
    let caller = mcp::Caller {
        step: header(&request, caos_world::secrets::STEP_HEADER).filter(|s| !s.trim().is_empty()),
        readers: header(&request, caos_world::secrets::READERS_HEADER).unwrap_or_default(),
        max_result_chars: header(&request, caos_world::secrets::MAX_RESULT_CHARS_HEADER)
            .and_then(|count| count.trim().parse().ok())
            .unwrap_or(mcp::DEFAULT_MAX_RESULT_CHARS),
    };
    let mut body = String::new();
    if let Err(error) = request.as_reader().read_to_string(&mut body) {
        return request.respond(
            Response::from_string(format!("reading request: {error}\n"))
                .with_status_code(StatusCode(400)),
        );
    }
    match mcp::handle(config, &caller, &body) {
        Some(message) => request.respond(
            Response::from_string(message.to_string()).with_header(
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                    .expect("a static header"),
            ),
        ),
        None => request.respond(Response::empty(StatusCode(202))),
    }
}

/// The secrets a request's SecretReaderKeys hold, for its session.
/// Headers, never the ArgTree: a key is a credential.
fn secret_context(config: &Config, request: &Request) -> Result<secrets::Context, HttpError> {
    let header = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().to_string())
    };
    secrets::Context::admit(
        config,
        &header(caos_world::secrets::READERS_HEADER).unwrap_or_default(),
        header(caos_world::secrets::SESSION_HEADER)
            .or_else(|| header(caos_world::secrets::LEGACY_SESSION_HEADER))
            .as_deref(),
    )
}

/// An expression's parameters: the query string, plus a form-style body when
/// the request has one.
fn expression_params(request: &mut Request, query: &str) -> Result<String, HttpError> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    Ok(join_params(query, &body))
}

/// `query` and `body` as one parameter string. A body is
/// `application/x-www-form-urlencoded`, in which `+` is a space (`curl
/// --data-urlencode` writes every space that way), so it is spelled `%20` here;
/// a literal `+` is `%2B` in a form, and still decodes to one. The query string
/// keeps the convention the rest of the server's URLs have, where `+` is
/// itself.
fn join_params(query: &str, body: &str) -> String {
    let body = body.trim().replace('+', "%20");
    match (query.is_empty(), body.is_empty()) {
        (_, true) => query.to_string(),
        (true, false) => body,
        (false, false) => format!("{query}&{body}"),
    }
}

/// A private repository's token for a `:@@=` fetch — the header
/// `POST /git/import` takes, for the same reason: it is the caller's, scoped
/// to one URL, and never looked up from the calling job's secrets.
fn git_token(request: &Request) -> Result<Option<String>, HttpError> {
    let tokens: Vec<_> = request
        .headers()
        .iter()
        .filter(|h| h.field.equiv(git_locator::import::TOKEN_HEADER))
        .map(|h| h.value.as_str().to_owned())
        .collect();
    if tokens.len() > 1 {
        return Err(HttpError::new(400, "duplicate Git token header"));
    }
    Ok(tokens.into_iter().next())
}

/// Match the request to a handler and produce the response body. Serves the
/// storage endpoints (`/object*`), compute (`/run`, `/sub-run`), and the runner
/// protocol (`/runner/poll`, `/runner/result`).
fn route(config: &Arc<Config>, request: &mut Request) -> Result<Vec<u8>, HttpError> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };

    match request.method() {
        Method::Get if path == "/run" => {
            let secrets = secret_context(config, request)?;
            compute::run(config, &query, &secrets)
        }
        Method::Get if path.starts_with("/status/") => {
            status::serve(config, path.trim_start_matches("/status/"), &query)
        }
        Method::Get if path == "/resolve-image" => compute::resolve_image_endpoint(config, &query),
        Method::Get if path == "/eval" => {
            let secrets = secret_context(config, request)?;
            let token = git_token(request)?;
            compute::eval_endpoint(config, &query, &secrets, token.as_deref())
        }
        // One `.caos-expr` line over HTTP: `/eval/run`, `/eval/curry`, and the
        // same for `/start/…`. The args ride in the query and, for a value too
        // big for one, in a POST body of the same `name:type=value&…` form.
        Method::Get | Method::Post if path.starts_with("/eval/") => {
            let params = expression_params(request, &query)?;
            let secrets = secret_context(config, request)?;
            compute::eval_expression_endpoint(
                config,
                path.trim_start_matches("/eval/"),
                &params,
                &secrets,
            )
        }
        // The same, for a request a client has bundled rather than spelled
        // (`POST /submit`'s body). `/wait/<ticket>` is what `/eval` does after
        // a start: it blocks for the ticket's answer.
        Method::Post if path == "/start" => {
            let secrets = secret_context(config, request)?;
            let token = git_token(request)?;
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body)?;
            compute::start_bundle(config, &body, &secrets, token.as_deref())
        }
        Method::Get if path.starts_with("/wait/") => {
            compute::wait_ticket(config, path.trim_start_matches("/wait/"))
        }
        Method::Post if path.starts_with("/start/") => {
            let params = expression_params(request, &query)?;
            let secrets = secret_context(config, request)?;
            compute::start_expression(
                config,
                path.trim_start_matches("/start/"),
                &params,
                &secrets,
            )
        }
        Method::Get if path.starts_with("/result/") => {
            compute::expression_result(config, path.trim_start_matches("/result/"))
        }
        Method::Post if path == "/submit" => {
            let secrets = secret_context(config, request)?;
            let token = git_token(request)?;
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body)?;
            compute::submit(config, &body, &secrets, token.as_deref())
        }
        Method::Get => match path.strip_prefix("/object/") {
            Some(hash) if !hash.is_empty() => storage::get_object(config, hash),
            _ => Err(HttpError::new(404, "not found")),
        },
        Method::Head => match path.strip_prefix("/object/") {
            Some(hash) if !hash.is_empty() => storage::head_object(config, hash),
            _ => Err(HttpError::new(404, "not found")),
        },
        Method::Post if path == "/object/" || path == "/object" => {
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body)?;
            storage::post_object(config, &body)
        }
        Method::Post if path == "/git/import" => import::endpoint(config, request),
        Method::Post if path == "/git/push" => push::endpoint(config, request),
        Method::Post if path == "/secrets/push" => secret_store::push_endpoint(config, request),
        Method::Post if path == "/sub-run" => {
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            runner::sub_run(&body)
        }
        Method::Post if path == "/trace/child" => {
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            runner::trace_child(config, &body)
        }
        Method::Post
            if path == "/runner/poll" || path == "/runner/result" || path == "/runner/lease" =>
        {
            let authorization = request
                .headers()
                .iter()
                .find(|h| h.field.to_string().eq_ignore_ascii_case("authorization"))
                .map(|h| h.value.to_string());
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            if path == "/runner/poll" {
                runner::poll(authorization.as_deref(), &body)
            } else if path == "/runner/lease" {
                runner::lease(authorization.as_deref(), &body)
            } else {
                runner::result(authorization.as_deref(), &body)
            }
        }
        _ => Err(HttpError::new(404, "not found")),
    }
}

/// How often stale request refs are swept, and how long one is kept.
const REQ_REF_SWEEP: std::time::Duration = std::time::Duration::from_secs(120);
const REQ_REF_KEEP: std::time::Duration = std::time::Duration::from_secs(600);

/// Sweep old `refs/caos/req/<hash>` refs in the background.
///
/// EVERY PUSHED OBJECT LEAVES ONE (`ensure_pushed`), so they accumulate without
/// bound — a single suite run adds about 1300 — and every push and fetch after
/// that pays for them, because git advertises the whole ref list on each
/// connection. Measured on this repo: 2560 refs is a 251 KB advertisement and a
/// 258ms push of one small object; at 1231 refs the same push is 181ms. That is
/// roughly 58ms per thousand refs, on every one of the hundreds of pushes a
/// suite makes, and it gets worse every run.
///
/// Safe to delete, on two independent grounds. Nothing READS them: the name is
/// written by `ensure_pushed` and never looked up (a result is fetched by hash,
/// or by the `refs/caos/res/<argTree>` the server pins). And they anchor
/// nothing: GC is off here precisely because almost every object is unreachable
/// from any ref, so these are not what keeps objects alive.
///
/// AGE, not a flush. Their one job is to be a negotiation base for the NEXT
/// push from the same client, seconds later, so that an edited tree ships only
/// its delta. Deleting a fresh one would make that client re-send a closure it
/// already sent, which is the cost this is trying to avoid.
fn spawn_request_ref_pruner(git_dir: String) {
    std::thread::spawn(move || loop {
        std::thread::sleep(REQ_REF_SWEEP);
        let dir = std::path::Path::new(&git_dir).join("refs/caos/req");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // No directory yet (nothing pushed) is the ordinary case, not news.
            Err(_) => continue,
        };
        let mut stale = String::new();
        let mut count = 0usize;
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t.elapsed().map(|age| age > REQ_REF_KEEP).unwrap_or(false))
                .unwrap_or(false);
            if !old {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                stale.push_str(&format!("delete refs/caos/req/{name}\n"));
                count += 1;
            }
        }
        if count == 0 {
            continue;
        }
        // Through git rather than by unlinking: it takes the same locks the
        // push path does, and it copes with a ref that has been packed since.
        let mut child = match std::process::Command::new("git")
            .envs(git_env())
            .args(["-C", &git_dir, "update-ref", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                eprintln!("caos-server: pruning request refs: {e}");
                continue;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(stale.as_bytes());
        }
        match child.wait() {
            Ok(_) => eprintln!("caos-server: pruned {count} stale request ref(s)"),
            Err(e) => eprintln!("caos-server: pruning request refs: {e}"),
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_form_body_decodes_plus_as_space_and_a_query_does_not() {
        use super::join_params;
        // curl --data-urlencode "cmd=echo written > out.txt"
        assert_eq!(
            join_params("a=1+2", "cmd=echo+written+%3E+out.txt&p=a%2Bb"),
            "a=1+2&cmd=echo%20written%20%3E%20out.txt&p=a%2Bb"
        );
        assert_eq!(join_params("a=1", ""), "a=1");
        assert_eq!(join_params("", " b=2 \n"), "b=2");
        assert_eq!(join_params("", ""), "");
    }

    use super::{
        configure_ref_advertisements, install_publish_hook, remove_managed_pre_receive_hook,
        run_required_git, MANAGED_HOOK_MARKERS,
    };
    use std::process::Command;

    #[test]
    fn required_git_command_reports_nonzero_status() {
        let absent = std::env::temp_dir().join(format!(
            "caos-required-git-test-{}-absent",
            std::process::id()
        ));
        std::fs::remove_dir_all(&absent).ok();
        let error = run_required_git(&[
            "-C",
            absent.to_str().unwrap(),
            "config",
            "http.receivepack",
            "true",
        ])
        .unwrap_err();
        assert!(error.contains("required command git"), "{error}");
        assert!(error.contains("exited with"), "{error}");
    }

    #[test]
    fn server_owned_refs_are_hidden_from_fetch_but_available_for_push_negotiation() {
        let dir = std::env::temp_dir().join(format!("caos-hidden-ref-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let git = |args: &[&str]| {
            let output = Command::new("git").args(args).output().unwrap();
            assert!(
                output.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        git(&["init", "-q", "--bare", dir.to_str().unwrap()]);
        git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "--add",
            "uploadpack.hideRefs",
            "refs/private/",
        ]);
        git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "--add",
            "receive.hideRefs",
            "refs/private-write/",
        ]);
        git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "--add",
            "receive.hideRefs",
            "refs/caos/res/",
        ]);
        let blob = git(&["-C", dir.to_str().unwrap(), "hash-object", "-w", "--stdin"]);
        let blob = blob.trim();
        for refname in [
            "refs/caos/req/request",
            "refs/caos/res/result",
            "refs/caos/v2/users/u-1/conversations/active/c-74657374",
            // A resolved locator's pin→tree memo (`mod locator`). One per pin, so
            // a server that has resolved a few hundred would advertise a few
            // hundred to every client that dials it.
            &format!("{}{}", crate::locator::TREE_REF_PREFIX, "a".repeat(40)),
        ] {
            git(&["-C", dir.to_str().unwrap(), "update-ref", refname, blob]);
        }
        configure_ref_advertisements(dir.to_str().unwrap()).unwrap();
        configure_ref_advertisements(dir.to_str().unwrap()).unwrap();
        let hidden = git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "--get-all",
            "uploadpack.hideRefs",
        ]);
        assert_eq!(hidden.lines().count(), 4, "{hidden}");
        assert!(hidden.lines().any(|value| value == "refs/private/"));
        let receive_hidden = git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "--get-all",
            "receive.hideRefs",
        ]);
        assert_eq!(receive_hidden.trim(), "refs/private-write/");

        let upload = git(&[
            "upload-pack",
            "--stateless-rpc",
            "--advertise-refs",
            dir.to_str().unwrap(),
        ]);
        assert!(!upload.contains("refs/caos/req/"));
        assert!(!upload.contains("refs/caos/res/"));
        assert!(!upload.contains(crate::locator::TREE_REF_PREFIX));
        assert!(upload.contains("refs/caos/v2/users/"));

        let receive = git(&[
            "receive-pack",
            "--stateless-rpc",
            "--advertise-refs",
            dir.to_str().unwrap(),
        ]);
        assert!(receive.contains("refs/caos/req/"));
        assert!(receive.contains("refs/caos/res/"));
        assert!(receive.contains("refs/caos/v2/users/"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn managed_pre_receive_hooks_are_removed_on_upgrade() {
        for (index, marker) in MANAGED_HOOK_MARKERS.iter().enumerate() {
            let dir = std::env::temp_dir().join(format!(
                "caos-managed-hook-test-{}-{index}",
                std::process::id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            run_required_git(&["init", "-q", "--bare", dir.to_str().unwrap()]).unwrap();
            let hooks = dir.join("hooks");
            let hook = hooks.join("pre-receive");
            std::fs::write(&hook, format!("#!/bin/sh\n{marker}\nexit 1\n")).unwrap();
            run_required_git(&[
                "-C",
                dir.to_str().unwrap(),
                "config",
                "core.hooksPath",
                hooks.to_str().unwrap(),
            ])
            .unwrap();

            remove_managed_pre_receive_hook(dir.to_str().unwrap()).unwrap();

            assert!(!hook.exists());
            let configured = Command::new("git")
                .args([
                    "-C",
                    dir.to_str().unwrap(),
                    "config",
                    "--get",
                    "core.hooksPath",
                ])
                .output()
                .unwrap();
            assert_eq!(configured.status.code(), Some(1));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn publish_hook_is_installed_and_refuses_a_foreign_one() {
        let dir =
            std::env::temp_dir().join(format!("caos-publish-hook-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        run_required_git(&["init", "-q", "--bare", dir.to_str().unwrap()]).unwrap();
        let git_dir = dir.to_str().unwrap();
        let hook = dir.join("hooks/pre-receive");

        install_publish_hook(git_dir).unwrap();
        let installed = std::fs::read_to_string(&hook).unwrap();
        assert!(installed.contains(crate::loose::HOOK_MARKER));
        assert!(installed.contains(crate::loose::HOOK_ARG));
        // Idempotent, and a stale managed copy is rewritten.
        install_publish_hook(git_dir).unwrap();
        let stale = format!("#!/bin/sh\n{}\nexit 0\n", crate::loose::HOOK_MARKER);
        std::fs::write(&hook, stale).unwrap();
        install_publish_hook(git_dir).unwrap();
        assert_eq!(std::fs::read_to_string(&hook).unwrap(), installed);

        // Without the server's name in its environment, the hook declines.
        let guard = format!("if [ -z \"${}\" ]", crate::loose::SERVER_EXE_ENV);
        assert!(installed.contains(&guard), "{installed}");

        // A hook that is not ours stops the server rather than being replaced.
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
        let error = install_publish_hook(git_dir).unwrap_err();
        assert!(error.contains("not caos's"), "{error}");
        std::fs::remove_file(&hook).unwrap();

        // So does a hooks path that sends git elsewhere.
        run_required_git(&["-C", git_dir, "config", "core.hooksPath", "/elsewhere"]).unwrap();
        let error = install_publish_hook(git_dir).unwrap_err();
        assert!(error.contains("core.hooksPath"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unmanaged_pre_receive_hook_and_path_are_preserved() {
        let dir =
            std::env::temp_dir().join(format!("caos-unmanaged-hook-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        run_required_git(&["init", "-q", "--bare", dir.to_str().unwrap()]).unwrap();
        let hooks = dir.join("hooks");
        let hook = hooks.join("pre-receive");
        let contents = b"#!/bin/sh\necho administrator hook\n";
        std::fs::write(&hook, contents).unwrap();
        run_required_git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "core.hooksPath",
            hooks.to_str().unwrap(),
        ])
        .unwrap();

        remove_managed_pre_receive_hook(dir.to_str().unwrap()).unwrap();

        assert_eq!(std::fs::read(&hook).unwrap(), contents);
        let configured = Command::new("git")
            .args([
                "-C",
                dir.to_str().unwrap(),
                "config",
                "--get",
                "core.hooksPath",
            ])
            .output()
            .unwrap();
        assert!(configured.status.success());
        assert_eq!(
            String::from_utf8(configured.stdout).unwrap().trim(),
            hooks.to_str().unwrap()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_managed_hook_path_is_removed_when_the_hook_is_already_absent() {
        let dir =
            std::env::temp_dir().join(format!("caos-stale-hook-path-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        run_required_git(&["init", "-q", "--bare", dir.to_str().unwrap()]).unwrap();
        let hooks = dir.join("hooks");
        run_required_git(&[
            "-C",
            dir.to_str().unwrap(),
            "config",
            "core.hooksPath",
            hooks.to_str().unwrap(),
        ])
        .unwrap();

        remove_managed_pre_receive_hook(dir.to_str().unwrap()).unwrap();

        let configured = Command::new("git")
            .args([
                "-C",
                dir.to_str().unwrap(),
                "config",
                "--get",
                "core.hooksPath",
            ])
            .output()
            .unwrap();
        assert_eq!(configured.status.code(), Some(1));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
