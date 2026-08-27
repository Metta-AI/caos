//! Compute: the `/run` pipeline.
//!
//! A **WorkRequest** (`SPEC.md`) is an **ArgTree** to run plus runtime context
//! (an ancestor `stack` for cycle detection) that is
//! NOT part of the cache key. The ArgTree is a content-addressed git tree, so its
//! hash *is* the cache key with nothing keyed alongside it: the worker image,
//! standard library `std`, and cache-busting `salt` all ride inside it under
//! reserved entries. `/run?req=<argTreeHash>`
//! reads it, then: cache lookup (Redis) → run-cycle detection → image
//! resolution (a digest-pinned `docker://` ref used as-is, or a git-docker image
//! converted and pushed to the registry) → dispatch through the runner
//! rendezvous ([`crate::runner`]:
//! the job is matched to a long-polling runner, which posts back
//! `"<type> <hash>"`) — or `"promise <hash>"`, a map-then continuation the
//! worker left behind instead of a value, which [`resolve_promise`] resolves
//! *after* the worker has moved on (see `design/map-then.md`). A top-level run
//! also pins `refs/caos/res/<argTreeHash>` at the (fully resolved) result.
//! Results, converted images, and built layers are all cached in Redis
//! (best-effort).
//!
//! Workers never wait on other workers: a worker either computes a value or
//! describes the remaining work (its promise) and finishes its job. Only server
//! threads block, so any number of concurrent runs cannot deadlock — capacity
//! lives runner-side (the set of parked polls), not in a server semaphore.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::storage::{fetch_blob, fetch_object, fetch_tree, store_git_blob, store_git_tree};
use crate::{Config, HttpError};

/// Repository name converted images are pushed under. They're addressed by
/// digest, so the name is arbitrary and fixed.
const REGISTRY_REPO: &str = "caos";

/// Prefix marking the `image` parameter as an ordinary docker reference rather
/// than one of our git images (the default).
const DOCKER_SCHEME: &str = "docker://";

/// Media type for the uncompressed-tar layers we build from git trees. Base
/// layers pulled from another registry keep their own (often gzipped) media type.
const OCI_LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar";

/// A manifest layer descriptor: `(media_type, digest, size)`.
type ManifestLayer = (String, String, u64);

/// A base image's contribution to a stacked image: its manifest layers and its
/// config `diff_id`s (the uncompressed layer digests) — the lower part of the
/// stack our delta layers sit on. Returned by [`fetch_base`].
type BaseLayers = (Vec<ManifestLayer>, Vec<String>);

/// How long to wait on Redis before giving up and running uncached.
const REDIS_TIMEOUT: Duration = Duration::from_secs(5);

/// Result type a worker reports when its output is a map-then continuation to
/// resolve rather than a final value (the hash names the continuation tree).
const PROMISE_KIND: &str = "promise";

/// Marker entry naming a curry node — a tree pairing a `base` image ref with an
/// `args` subtree of bound arguments (mirrors the client's `CURRY_MARKER`).
/// Promise resolution unwraps these server-side so `map`/`then` can be
/// curried images.
const CURRY_MARKER: &str = ".caos-curry";

/// Reserved suffix for the per-entry permission sidecars `import-image` writes.
const META_SUFFIX: &str = ".caosmeta";

/// Disambiguates temp dirs created across handler threads.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A **WorkRequest** (per `SPEC.md`): an `ArgTree` to run, plus the runtime
/// context that is deliberately NOT part of its cache key — the ancestor `stack`
/// (run-cycle detection). Only `arg_tree` is hashed and
/// cached; `stack` rides alongside it. The ArgTree carries the
/// worker image, std and salt under reserved entries, so its hash *is* the whole
/// cache key (`SPEC.md`: "The ArgTree is the cache key").
#[derive(Clone, Copy)]
struct WorkRequest<'a> {
    /// The ArgTree hash — the request's identity and cache key.
    arg_tree: &'a str,
    /// Ancestor ArgTree hashes (empty = top-level), for run-cycle detection.
    stack: &'a [String],
    /// The secrets the run carries as ephemeral context (design/secrets.md):
    /// matched + injected at every dispatch, threaded into promise sub-runs.
    /// NOT part of the ArgTree or the cache key.
    secrets: &'a [crate::secrets::Grant],
}

/// `GET /run?req=<argTreeHash>` — run the ArgTree `<argTreeHash>` (which carries
/// the worker image, std and salt under reserved entries) and return its result
/// as `"<type> <hash>"`. (`req` is the query param's historical name; its value
/// is the ArgTree hash.)
///
/// The ArgTree being a content-addressed object means its hash *is* the cache key
/// (it captures everything — image, std, salt and the rest)
/// and the rendezvous id: an external run also pins
/// `refs/caos/res/<argTreeHash>` at the result, so a client can fetch it by ref.
/// Most worker sub-runs are promise resolutions the server performs itself
/// ([`run_work_request`] recursion). Detached worker work enters through
/// `POST /sub-run`, which starts this same pipeline with the launching job's
/// existing stack and secret store.
pub(crate) fn run(
    config: &Config,
    query: &str,
    secrets_header: &str,
) -> Result<Vec<u8>, HttpError> {
    let arg_tree = parse_arg_tree(query)?;
    // The carried secrets store (design/secrets.md): parsed from the request
    // header, held for this run and threaded through every sub-run's dispatch.
    let secrets = crate::secrets::parse_header(secrets_header);
    // An HTTP run is by definition top-level: the run stack (cycle detection)
    // exists only inside the server, threaded through promise sub-runs.
    let result = run_work_request(
        config,
        &WorkRequest {
            arg_tree: &arg_tree,
            stack: &[],
            secrets: &secrets,
        },
    );
    // Top-level publication is part of `run_work_request`: a flight owner pins
    // before releasing its ownership, and waiters receive that combined
    // compute-and-publication outcome.
    let result = result?;
    Ok(format!("{result}\n").into_bytes())
}

/// Parse and validate the request identity.
fn parse_arg_tree(query: &str) -> Result<String, HttpError> {
    let arg_tree = query_param(query, "req")
        .ok_or_else(|| HttpError::new(400, "missing 'req' query parameter"))?;
    if arg_tree.len() != 40
        || !arg_tree
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(HttpError::new(
            400,
            format!("invalid arg-tree hash: {arg_tree:?}"),
        ));
    }
    Ok(arg_tree)
}

/// Run WorkRequest `request` (its ArgTree, with `request.stack` the chain of
/// ancestor ArgTree hashes — empty = top-level), returning the fully-resolved
/// `"<type> <hash>"`. The whole pipeline behind both `GET /run` and promise
/// sub-runs: cache lookup → run-cycle detection → the container run → promise
/// resolution → cache store → top-level result publication.
/// The redis key an ArgTree's result is cached under.
///
/// Unprefixed when no namespace is configured, so a deployment that sets nothing
/// keeps the keys it already has and nothing has to be migrated.
fn result_key(config: &Config, arg_tree: &str) -> String {
    if config.cache_namespace.is_empty() {
        format!("caos:result:{arg_tree}")
    } else {
        format!("caos:result:{}:{arg_tree}", config.cache_namespace)
    }
}

fn run_work_request(config: &Config, request: &WorkRequest) -> Result<String, HttpError> {
    let WorkRequest {
        arg_tree,
        stack,
        secrets: _,
    } = *request;
    // Unpack the ArgTree's reserved worker `base` (an embedded tree for a git
    // image, a ref blob for `docker://`) and cache-busting `salt`. Both are part
    // of the ArgTree — hence part of the cache key — and inherited by any
    // promise sub-runs this request leaves behind.
    let (image, salt) = read_arg_tree(config, arg_tree)?;
    if image.is_empty() {
        return Err(HttpError::new(400, "request has empty image"));
    }

    // The ArgTree hash is the cache key (it captures image, std, salt and every
    // other arg), NAMESPACED by the stack — see `Config::cache_namespace`, which
    // covers the one thing the ArgTree cannot: which build of caos produced the
    // answer. The value is
    // the final result "<type> <hash>" — a promise is resolved before it's cached,
    // so a hit never re-resolves. A hit skips image conversion and the container
    // run. Redis is best-effort: a lookup error just means we run uncached.
    let key = &result_key(config, arg_tree);
    match cache_get(&config.redis_addr, key) {
        Ok(Some(result)) => {
            eprintln!("cache hit: arg_tree={arg_tree} -> {result}");
            let outcome = complete_cache_hit(arg_tree, stack, result, |result| {
                pin_result(config, arg_tree, result)
            });
            return outcome.map_err(|(status, msg)| HttpError::new(status, msg));
        }
        Ok(None) => eprintln!("cache miss: arg_tree={arg_tree} (image={image}); running worker"),
        Err(e) => eprintln!("cache lookup failed ({e}); running worker: arg_tree={arg_tree}"),
    }

    // Re-entering an ArgTree already on the stack has no fixpoint — fail, listing
    // the cycle. (A cache hit can't be on the stack: a cyclic computation never
    // completes, so it never caches, which is why checking only on a miss is
    // sound.) The ArgTree hash is exactly this frame's identity.
    if let Some(pos) = stack.iter().position(|f| f == arg_tree) {
        let mut cycle: Vec<&str> = stack[pos..].iter().map(String::as_str).collect();
        cycle.push(arg_tree);
        let listing = cycle.join("\n  -> ");
        eprintln!("run cycle detected:\n  {listing}");
        return Err(HttpError::new(
            400,
            format!("run cycle detected:\n  {listing}"),
        ));
    }

    // Single-flight: identical concurrent requests share one run. A diamond
    // DAG (parallel map children with a shared dependency) otherwise computes
    // the shared node once per concurrent parent — same result, wasted
    // containers. The first arrival runs; later ones park on a channel and
    // get the same final result.
    //
    // Parking needs a waits-for check (predicted in design/map-then.md): in a
    // CYCLIC graph, the in-flight run we'd park on can transitively depend on
    // one of OUR ancestors — parking then closes a cross-thread wait cycle
    // the local stack check above cannot see (the old always-duplicate
    // behavior escaped it by re-running until the repeat landed on one
    // thread's stack). `park_would_deadlock` walks the parked-waiter graph
    // for exactly that reachability; an unsafe arrival runs independently —
    // the duplicate grows its descendants' stacks and the genuine cycle then
    // errors cleanly. An owner guard clears and broadcasts if its thread
    // unwinds; a waiter never promotes itself merely because a valid run is
    // slow, since duplicate execution may repeat external side effects.
    let (outcome, owner) = match claim_flight_after_miss(arg_tree, stack, || {
        cache_get(&config.redis_addr, key)
    }) {
        FlightDisposition::Run(owner) => (run_dispatch(config, request, &image, &salt, key), owner),
        FlightDisposition::Complete {
            outcome,
            cache_hit,
            owner,
        } => {
            // A hit HERE is the post-claim re-read catching a result that
            // landed while this arrival was between its own miss and the
            // flight — the near-duplicate the re-read exists to prevent.
            // Logged beside the ordinary hit/miss lines because it is the
            // only place that saving is visible; the trace records nothing
            // for it, since this arrival performed no work.
            if cache_hit {
                eprintln!("cache hit after claiming the flight: arg_tree={arg_tree}");
            }
            (outcome, owner)
        }
    };
    let outcome = match owner {
        // Any top-level participant marks the shared flight for publication,
        // even when its executor entered as a promise sub-run. The owner keeps
        // the flight fenced until that publication finishes.
        Some(owner) => owner.finish_with(outcome, |result| pin_result(config, arg_tree, result)),
        // A waiter receives the owner's already-published outcome. The only
        // ownerless executor is a cycle-breaking sub-run: an external request
        // has no ancestors, so it can never take the unsafe path.
        None => outcome,
    };
    outcome.map_err(|(status, msg)| HttpError::new(status, msg))
}

/// The dispatch + promise-resolution + cache-store tail of [`run_work_request`],
/// factored out so single-flight can broadcast its outcome. The error type is
/// `(status, message)` — a plain-data [`HttpError`] that can be cloned to every
/// waiter.
///
/// This is also the whole of the traced path (SPEC.md "Tracing"): only an
/// arrival that actually RUNS the work gets here, so a cache hit and a
/// single-flight waiter record nothing. That is what makes the reader's
/// inference sound — a child whose record ended before its parent started was
/// reused rather than re-run, and it would not be if every consumer appended.
fn run_dispatch(
    config: &Config,
    request: &WorkRequest,
    image: &str,
    salt: &str,
    key: &str,
) -> Result<String, (u16, String)> {
    // `requested` before anything else, `ended` however we leave — including
    // the continuation resolution inside, which is why this wraps the whole
    // body rather than sitting beside the runner dispatch. A promise's children
    // are started after the worker has exited, so an `ended` that stopped at
    // the container would place every child after its parent's end and make
    // the reader call all of them evicted-and-rerun.
    crate::status::requested(config, request.arg_tree);
    let outcome = run_dispatch_inner(config, request, image, salt, key);
    crate::status::ended(config, request.arg_tree, outcome.is_ok());
    outcome
}

fn run_dispatch_inner(
    config: &Config,
    request: &WorkRequest,
    image: &str,
    salt: &str,
    key: &str,
) -> Result<String, (u16, String)> {
    let WorkRequest {
        arg_tree,
        stack,
        secrets,
    } = *request;
    let fail = |e: HttpError| (e.status(), e.message().to_string());

    // Promise sub-runs see this computation as an ancestor.
    let mut child_stack: Vec<String> = stack.to_vec();
    child_stack.push(arg_tree.to_string());

    // Run the worker through the runner rendezvous: resolve the image to a
    // docker-pullable ref (always sent — a warm runner that pinned the image
    // ignores it, and conversion is Redis-cached, so re-resolving is a lookup),
    // read the ArgTree's top level (what runners' required args match
    // against), and hand the job to a polling runner. The dispatch blocks this
    // server thread until a runner posts the result; capacity is runner-side
    // (the set of parked polls), so there's no server-side slot to hold.
    let result = {
        let image_ref = resolve_image(config, image).map_err(fail)?;
        let arg_entries = args_entries(config, arg_tree).map_err(fail)?;
        // Whether this job SAYS it is answered by a seeder, which it does by
        // carrying `required-pool=seeded` — the same arg that keeps the generic
        // pool away from it (`runner::matches`). It used to be sniffed from the
        // image being a `docker://seeded…` sentinel, which named only three of
        // the five seeded entries: `cargo`'s image is the flake-builder delta
        // and `flake-builder`'s own is a plain sentinel, so a cargo job whose
        // key had drifted looked like ordinary work. It was then handed to a
        // generic runner, which really tried to `nix build` a flake that
        // deliberately exposes no `caosImage` — a failure naming nix, three
        // layers from the stale key that caused it.
        let seeded = arg_entries
            .get(caos_world::REQUIRED_POOL_ARG)
            .is_some_and(|oid| *oid == crate::runner::pool_oid(caos_world::SEEDED_POOL));
        // Find the secrets this job's identity is entitled to (design/secrets.md):
        // carried readers whose partial arg tree is a subset of ours. They ride
        // out of band in the job payload — never in the ArgTree, so never in the
        // cache key — and the container runner drops them at `/secret/<name>`.
        // Read from `arg_entries` before dispatch takes ownership of it.
        let granted = crate::secrets::grant(secrets, &arg_entries);
        crate::runner::dispatch(
            arg_tree,
            arg_entries,
            &image_ref,
            seeded,
            granted,
            |sub_request| start_sub_run(config, sub_request, &child_stack, secrets),
            |note| match note {
                crate::runner::Note::Started => crate::status::started(config, arg_tree),
                crate::runner::Note::OutTrace(oid) => {
                    crate::status::out_trace(config, arg_tree, &oid)
                }
            },
        )
        .map_err(fail)?
    };

    if result_hash(&result).is_empty() {
        eprintln!("worker produced no result on stdout: arg_tree={arg_tree}");
        return Err((500, "worker produced no result on stdout".to_string()));
    }

    // A promise is not a value: the worker exited leaving a map-then continuation
    // behind. Resolve it — the container (and its slot) are already gone.
    let (result, caught) = match result.split_once(' ') {
        Some((PROMISE_KIND, cont)) => {
            eprintln!("resolving promise: arg_tree={arg_tree} -> continuation {cont}");
            resolve_promise(config, arg_tree, cont, salt, &child_stack, secrets).map_err(fail)?
        }
        _ => (result, false),
    };

    // Cache the (resolved) result for next time (best-effort) — unless a `catch`
    // folded a sub-run failure into it. A failed sub-run is deliberately never
    // cached; memoizing the parent that swallowed it would reintroduce exactly
    // the memoized red that rule exists to prevent, and make it unretryable.
    if caught {
        eprintln!("ran worker: arg_tree={arg_tree} -> {result} (not cached: caught a failure)");
    } else {
        match cache_set(&config.redis_addr, key, &result) {
            Ok(()) => eprintln!("ran worker: arg_tree={arg_tree} -> {result} (cached)"),
            Err(e) => {
                eprintln!("ran worker: arg_tree={arg_tree} -> {result} (cache store failed: {e})")
            }
        }
    }

    Ok(result)
}

/// Admit an exact detached child request while the parent job is still in
/// flight. Ownership crosses the thread boundary here: the server clones the
/// current unhashed context, while the worker sends only the child hash and its
/// job-scoped nonce. Validation happens before acknowledgement so a missing or
/// malformed request is not reported as queued.
fn start_sub_run(
    config: &Config,
    arg_tree: &str,
    stack: &[String],
    secrets: &[crate::secrets::Grant],
) -> Result<(), HttpError> {
    let (image, _) = read_arg_tree(config, arg_tree)?;
    if image.is_empty() {
        return Err(HttpError::new(400, "sub-run request has empty image"));
    }

    let config = config.clone();
    let arg_tree = arg_tree.to_string();
    let stack = stack.to_vec();
    let secrets = secrets.to_vec();
    std::thread::spawn(move || {
        let result = run_work_request(
            &config,
            &WorkRequest {
                arg_tree: &arg_tree,
                stack: &stack,
                secrets: &secrets,
            },
        );
        match result {
            Ok(result) => eprintln!("sub-run completed: arg_tree={arg_tree} -> {result}"),
            Err(error) => eprintln!("sub-run failed: arg_tree={arg_tree}: {}", error.message()),
        }
    });
    Ok(())
}

// ---- Single-flight -----------------------------------------------------------

/// A run's outcome in plain data, so it can be sent to every parked waiter.
type Outcome = Result<String, (u16, String)>;

struct FlightEntry {
    waiters: Vec<mpsc::Sender<Outcome>>,
    /// At least one participant is a top-level HTTP run and therefore needs
    /// the result pinned before the flight completes.
    publish_result: bool,
}

/// In-flight runs: ArgTree hash → the flight's waiters and publication need.
fn flights() -> &'static Mutex<HashMap<String, FlightEntry>> {
    static FLIGHTS: OnceLock<Mutex<HashMap<String, FlightEntry>>> = OnceLock::new();
    FLIGHTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One parked waiter: its ancestor stack, and the ArgTree it waits on — an
/// edge of the waits-for graph `park_would_deadlock` walks. The ID prevents a
/// delayed guard from removing an identical edge belonging to a later flight.
struct ParkedEdge {
    id: u64,
    stack: Vec<String>,
    target: String,
}

struct ParkedState {
    next_id: u64,
    edges: Vec<ParkedEdge>,
}

/// Every parked waiter, as the edges of that graph.
fn parked() -> &'static Mutex<ParkedState> {
    static PARKED: OnceLock<Mutex<ParkedState>> = OnceLock::new();
    PARKED.get_or_init(|| {
        Mutex::new(ParkedState {
            next_id: 0,
            edges: Vec::new(),
        })
    })
}

/// Would parking a waiter (ancestry `stack`) on in-flight `arg_tree` close a wait
/// cycle? True iff `arg_tree`'s in-progress subtree — reached transitively through
/// parked waiters (a waiter whose ancestry contains a frontier ArgTree descends
/// from it, so the frontier ArgTree's completion awaits the waiter's target) —
/// contains one of OUR ancestors: that ancestor's completion awaits us, so
/// waiting on `arg_tree` would deadlock.
fn park_would_deadlock(arg_tree: &str, stack: &[String]) -> bool {
    let parked = parked().lock().expect("parked lock");
    let mut frontier: Vec<String> = vec![arg_tree.to_string()];
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(arg_tree.to_string());
    while let Some(cur) = frontier.pop() {
        if stack.contains(&cur) {
            return true;
        }
        for edge in &parked.edges {
            if edge.stack.contains(&cur) && seen.insert(edge.target.clone()) {
                frontier.push(edge.target.clone());
            }
        }
    }
    false
}

enum Flight {
    /// Nobody is running this request: run it (and `finish_flight` after).
    Owner(FlightOwner),
    /// Someone is: park here for their outcome. The guard unregisters the
    /// waits-for edge when the wait ends (either way).
    Waiter(mpsc::Receiver<Outcome>, ParkGuard),
    /// Someone is, but parking would deadlock: run independently.
    Unsafe,
}

enum FlightDisposition {
    /// This arrival must execute. `Some` owns the canonical flight; `None` is
    /// the independent duplicate required to expose a cross-thread cycle.
    Run(Option<FlightOwner>),
    /// A live owner or the post-claim cache re-read supplied the outcome. The
    /// latter retains its owner so top-level publication happens before the
    /// flight is released.
    Complete {
        outcome: Outcome,
        cache_hit: bool,
        owner: Option<FlightOwner>,
    },
}

/// Join the flight after an initial cache miss, then re-read the cache if this
/// arrival will execute. A miss can be descheduled before it reaches the flight
/// table; the previous owner may finish, cache, and remove its entry meanwhile.
/// The new owner must therefore check again while its ownership excludes any
/// other ordinary in-process executor. Unsafe cycle-breaking duplicates also
/// re-read before repeating possibly effectful work.
fn claim_flight_after_miss(
    arg_tree: &str,
    stack: &[String],
    reread_cache: impl FnOnce() -> Result<Option<String>, String>,
) -> FlightDisposition {
    match join_flight(arg_tree, stack) {
        Flight::Owner(owner) => match reread_cache() {
            Ok(Some(result)) => {
                eprintln!("cache hit after single-flight claim: arg_tree={arg_tree} -> {result}");
                let outcome = Ok(result);
                FlightDisposition::Complete {
                    outcome,
                    cache_hit: true,
                    owner: Some(owner),
                }
            }
            Ok(None) => FlightDisposition::Run(Some(owner)),
            Err(error) => {
                eprintln!(
                    "cache lookup after single-flight claim failed ({error}); running worker: arg_tree={arg_tree}"
                );
                FlightDisposition::Run(Some(owner))
            }
        },
        Flight::Unsafe => match reread_cache() {
            Ok(Some(result)) => {
                eprintln!("cache hit before cycle-breaking run: arg_tree={arg_tree} -> {result}");
                FlightDisposition::Complete {
                    outcome: Ok(result),
                    cache_hit: true,
                    owner: None,
                }
            }
            Ok(None) => {
                eprintln!(
                    "single-flight: arg_tree={arg_tree} parking would deadlock; running independently"
                );
                FlightDisposition::Run(None)
            }
            Err(error) => {
                eprintln!(
                    "cache lookup before cycle-breaking run failed ({error}); running independently: arg_tree={arg_tree}"
                );
                FlightDisposition::Run(None)
            }
        },
        Flight::Waiter(rx, guard) => {
            let outcome = wait_for_flight(arg_tree, rx, guard);
            FlightDisposition::Complete {
                outcome,
                cache_hit: false,
                owner: None,
            }
        }
    }
}

/// Complete a cache hit without letting an external caller bypass a live
/// flight. A top-level hit either owns a short publication-only flight or waits
/// for the existing owner's newer compute-and-publication outcome. Promise
/// sub-runs keep the direct cache-hit path: they publish no durable result ref,
/// and joining after a hit would add waits-for edges that cache resolution does
/// not need.
fn complete_cache_hit(
    arg_tree: &str,
    stack: &[String],
    result: String,
    publish: impl FnOnce(&str) -> Result<(), HttpError>,
) -> Outcome {
    if !stack.is_empty() {
        return Ok(result);
    }
    match join_flight(arg_tree, stack) {
        Flight::Owner(owner) => owner.finish_with(Ok(result), publish),
        Flight::Waiter(rx, guard) => wait_for_flight(arg_tree, rx, guard),
        Flight::Unsafe => Err((
            500,
            format!("top-level cache hit for {arg_tree} could not join its result flight"),
        )),
    }
}

fn wait_for_flight(arg_tree: &str, rx: mpsc::Receiver<Outcome>, guard: ParkGuard) -> Outcome {
    let outcome = rx.recv();
    drop(guard);
    match outcome {
        Ok(outcome) => {
            eprintln!("single-flight: arg_tree={arg_tree} joined an in-flight run");
            outcome
        }
        Err(_) => Err((
            500,
            format!("single-flight owner for {arg_tree} ended without an outcome"),
        )),
    }
}

/// The unique owner of one in-process flight. Normal completion explicitly
/// broadcasts its outcome. Unwinding is a provable owner loss, so Drop clears
/// the entry and wakes waiters with an error; a later request may then own it.
struct FlightOwner {
    arg_tree: String,
    finished: bool,
}

impl FlightOwner {
    #[cfg(test)]
    fn finish(mut self, outcome: &Outcome) {
        finish_flight(&self.arg_tree, outcome);
        self.finished = true;
    }

    /// If an external participant requested durable publication, publish while
    /// this owner remains in the flight table, then broadcast the combined
    /// outcome. A publication failure is therefore seen by every waiter.
    fn finish_with(
        mut self,
        outcome: Outcome,
        publish: impl FnOnce(&str) -> Result<(), HttpError>,
    ) -> Outcome {
        let outcome = if reserve_publication_or_finish(&self.arg_tree, &outcome) {
            match outcome {
                Ok(result) => match publish(&result) {
                    Ok(()) => Ok(result),
                    Err(error) => Err((error.status(), error.message().to_string())),
                },
                Err(error) => Err(error),
            }
        } else {
            // `reserve_publication_or_finish` already removed the flight and
            // broadcast this outcome atomically with deciding no pin was needed.
            self.finished = true;
            return outcome;
        };
        finish_flight(&self.arg_tree, &outcome);
        self.finished = true;
        outcome
    }
}

impl Drop for FlightOwner {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        finish_flight(
            &self.arg_tree,
            &Err((
                500,
                format!("single-flight owner for {} was lost", self.arg_tree),
            )),
        );
    }
}

/// Removes this waiter's waits-for edge on drop.
struct ParkGuard {
    id: u64,
}

impl Drop for ParkGuard {
    fn drop(&mut self) {
        let mut parked = parked().lock().expect("parked lock");
        if let Some(pos) = parked.edges.iter().position(|edge| edge.id == self.id) {
            parked.edges.swap_remove(pos);
        }
    }
}

fn join_flight(arg_tree: &str, stack: &[String]) -> Flight {
    let mut table = flights().lock().expect("flights lock");
    match table.get_mut(arg_tree) {
        Some(flight) => {
            // The waits-for check runs under the flights lock, so a
            // concurrent park can't slip in between the check and the edge
            // registration.
            if park_would_deadlock(arg_tree, stack) {
                return Flight::Unsafe;
            }
            let (tx, rx) = mpsc::channel();
            let id = {
                let mut parked = parked().lock().expect("parked lock");
                let id = parked.next_id;
                parked.next_id = parked
                    .next_id
                    .checked_add(1)
                    .expect("parked waiter ID space exhausted");
                parked.edges.push(ParkedEdge {
                    id,
                    stack: stack.to_vec(),
                    target: arg_tree.to_string(),
                });
                id
            };
            flight.waiters.push(tx);
            // An external request may join a flight whose executor is a
            // promise sub-run. Record its need here so that executor publishes
            // before waking it.
            if stack.is_empty() {
                flight.publish_result = true;
            }
            Flight::Waiter(rx, ParkGuard { id })
        }
        None => {
            table.insert(
                arg_tree.to_string(),
                FlightEntry {
                    waiters: Vec::new(),
                    publish_result: stack.is_empty(),
                },
            );
            Flight::Owner(FlightOwner {
                arg_tree: arg_tree.to_string(),
                finished: false,
            })
        }
    }
}

/// Decide, under the flight-table lock, whether successful completion needs a
/// publication phase. If it does, leave the entry present so new arrivals keep
/// waiting while the ref is pinned. Otherwise remove and broadcast now; an
/// external request racing this decision either marks the existing flight
/// first or becomes the next owner after removal.
fn reserve_publication_or_finish(arg_tree: &str, outcome: &Outcome) -> bool {
    let waiters = {
        let mut table = flights().lock().expect("flights lock");
        if outcome.is_ok()
            && table
                .get(arg_tree)
                .is_some_and(|flight| flight.publish_result)
        {
            return true;
        }
        take_flight(&mut table, arg_tree)
    };
    broadcast(waiters, outcome);
    false
}

/// Broadcast the outcome to every parked waiter and clear the entry. Only the
/// owner calls this; an unsafe cycle-breaking duplicate must not steal its
/// waiters or publish a cycle error as the canonical flight outcome.
fn finish_flight(arg_tree: &str, outcome: &Outcome) {
    let waiters = {
        let mut table = flights().lock().expect("flights lock");
        take_flight(&mut table, arg_tree)
    };
    broadcast(waiters, outcome);
}

fn take_flight(
    table: &mut HashMap<String, FlightEntry>,
    arg_tree: &str,
) -> Vec<mpsc::Sender<Outcome>> {
    let waiters = table
        .remove(arg_tree)
        .map(|flight| flight.waiters)
        .unwrap_or_default();
    // A sent outcome no longer waits on this flight, even if the receiving
    // thread has not yet been scheduled to drop its ParkGuard. Remove the
    // completed edges while still holding the flights lock so a new flight for
    // the same key cannot register an edge that this completion would mistake
    // for one of its own.
    parked()
        .lock()
        .expect("parked lock")
        .edges
        .retain(|edge| edge.target != arg_tree);
    waiters
}

fn broadcast(waiters: Vec<mpsc::Sender<Outcome>>, outcome: &Outcome) {
    for tx in waiters {
        let _ = tx.send(outcome.clone());
    }
}

// ---- Promise resolution ------------------------------------------------------

/// The SERVER backend for the shared `.caos-expr` walk (crate `caos-eval`): CAS
/// over the in-process object database, and a `run` dispatched by [`run_image`]
/// — which blocks THIS request thread (never a worker slot), exactly as
/// [`resolve_promise`]'s own `run` step does. This is what lets a worker request
/// evaluation server-side, via an `eval` continuation, and get the byte-identical
/// object a client `eval-path` would build. Marking is a no-op: server-side
/// secret marking through eval is deferred (tools use no secrets).
struct ServerEvalHost<'a> {
    config: &'a Config,
    salt: &'a str,
    stack: &'a [String],
    secrets: &'a [crate::secrets::Grant],
    /// The promising ArgTree whose continuation this walk is, and the path it
    /// was asked to evaluate — together the name under which each dispatch is
    /// recorded as that node's child.
    ///
    /// An eval step's fan-out is NOT one child per directory: `eval_path` is a
    /// loop on this thread whose `--base`/arg resolution recurses, and a `run`
    /// verb at any depth dispatches (`caos-eval`). So the count comes from the
    /// interpreter, not from anything in the continuation, and the only way to
    /// know what ran is to record each dispatch as it goes.
    parent: &'a str,
    eval_path: &'a str,
    dispatches: std::sync::atomic::AtomicUsize,
}

impl caos_eval::EvalHost for ServerEvalHost<'_> {
    fn get_object(&self, oid: &str) -> Result<(String, Vec<u8>), String> {
        fetch_object(self.config, oid)
    }
    fn post_object(&self, kind: &str, bytes: &[u8]) -> Result<gix::ObjectId, String> {
        if kind != "blob" {
            return Err(format!("eval host can only post blobs, got {kind}"));
        }
        store_git_blob(self.config, bytes)
    }
    fn fetch_tree_entries(
        &self,
        tree: &str,
    ) -> Result<Option<Vec<gix::objs::tree::Entry>>, String> {
        // Match the client's fetch_tree_entries exactly: `None` for a non-tree,
        // parsed straight from the object bytes (one fetch).
        let (kind, content) = fetch_object(self.config, tree)?;
        if kind != "tree" {
            return Ok(None);
        }
        let parsed = gix::objs::TreeRef::from_bytes(&content, gix::hash::Kind::Sha1)
            .map_err(|e| format!("malformed tree {tree}: {e}"))?;
        Ok(Some(
            parsed
                .entries
                .iter()
                .map(|e| gix::objs::tree::Entry {
                    mode: e.mode,
                    filename: e.filename.to_vec().into(),
                    oid: e.oid.to_owned(),
                })
                .collect(),
        ))
    }
    fn post_tree(&self, entries: Vec<gix::objs::tree::Entry>) -> Result<gix::ObjectId, String> {
        store_git_tree(self.config, entries)
    }
    fn dispatch(
        &self,
        image: &str,
        entries: Vec<gix::objs::tree::Entry>,
    ) -> Result<(String, String), String> {
        let n = self
            .dispatches
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let result = run_image(
            self.config,
            image,
            entries,
            self.salt,
            self.stack,
            self.secrets,
            |arg_tree| {
                crate::status::child(
                    self.config,
                    self.parent,
                    "eval",
                    &format!("{}#{n}", self.eval_path),
                    arg_tree,
                )
            },
        )
        .map_err(|e| e.message().to_string())?;
        // run_image returns "<kind> <hash>".
        let (kind, hash) = result
            .split_once(' ')
            .ok_or_else(|| format!("malformed run result {result:?}"))?;
        Ok((kind.to_string(), hash.to_string()))
    }
}

/// Resolve a continuation — either `{in, map?|run?|then?, catch?}` or
/// `{request, then?, catch?}`. `request` is a tree entry naming an already
/// complete ArgTree, executed unchanged; `map`/`run`/`then` are blobs naming
/// images. The four middle forms `map`/`run`/`request`/`eval` are mutually exclusive
/// (the client already refuses to record both; this is defense in depth). One
/// resolution path covers both forms — a *middle step*, then `then`:
///
/// 1. if `map` is given: run `map --in=<child>` for each child of `in` in
///    parallel (a blob `in` is a leaf — no children), assembling the results
///    into a `children` tree under the original names;
/// 2. if `run` is given: one sub-run, `run(--in=<in>)` — the single-valued
///    form. Its result R may be any kind (a commit as much as a blob/tree);
/// 3. if `request` is given: run exactly that ArgTree, adding no args;
/// 4. if `eval` is given: walk `.caos-expr` from `in`'s root down to that path,
///    server-side, and its result R is the value (see `ServerEvalHost`);
/// 5. `then` receives the available args: `--in` for the image forms, plus
///    `--children`/`--result`; the exact-request form passes only `--result`.
///
/// Every sub-run goes through [`run_work_request`], so promises nest arbitrarily (a map
/// child, a `run`, or a `then` may itself promise) and each sub-run gets its
/// own memoization and cycle detection (via `stack`).
///
/// **`catch`** (a marker blob, `run`, `request` or `eval`) makes a failing middle step a
/// value `then` receives as `--error=<blob>`, exactly where `--result` would
/// have been. Without it a failed sub-run fails the whole
/// request, which is the right default for a pipeline — but wrong for a driver
/// that must survive its callee, the agent loop being the case that forced it
/// (`design/agent-harness.md`, "Tool failures are values"). A caught `map`
/// would have to say WHICH child failed and what the surviving siblings'
/// results mean, so catch remains single-valued (`run`, `request` or `eval`).
///
/// The bool in the return says a catch fired. It rides out to [`run_dispatch`]
/// so the enclosing request is NOT memoized: sub-run failures are uncached by
/// design (`design/cargo-workers.md`), and folding one into a cached parent
/// result would launder it into a permanent answer — a retry would replay the
/// failure without re-running anything. `then`'s own request still caches
/// normally; the error blob is in its ArgTree, so same error in, same out.
fn resolve_promise(
    config: &Config,
    parent: &str,
    cont: &str,
    salt: &str,
    stack: &[String],
    secrets: &[crate::secrets::Grant],
) -> Result<(String, bool), HttpError> {
    use gix::objs::tree::EntryKind;

    let mut input: Option<gix::objs::tree::Entry> = None;
    let mut request: Option<String> = None;
    let (mut map, mut run, mut then) = (None, None, None);
    let mut eval = None;
    let mut catch = false;
    for entry in fetch_tree(config, cont)
        .map_err(|e| HttpError::new(500, format!("reading continuation {cont}: {e}")))?
    {
        match entry.name.as_str() {
            "in" => input = Some(named_entry("in", entry.mode, entry.oid)),
            "request" if entry.mode.is_tree() => request = Some(entry.oid.to_string()),
            "request" => {
                return Err(HttpError::new(
                    500,
                    format!("continuation {cont} has a non-tree 'request' entry"),
                ))
            }
            "map" => map = Some(blob_string(config, &entry.oid.to_string())?),
            "run" => run = Some(blob_string(config, &entry.oid.to_string())?),
            "then" => then = Some(blob_string(config, &entry.oid.to_string())?),
            // The PATH to evaluate (a blob, not an image): the middle step walks
            // `.caos-expr` from `in`'s root down to it. See `ServerEvalHost`.
            "eval" => eval = Some(blob_string(config, &entry.oid.to_string())?),
            // Presence is the whole signal; the content is unread.
            "catch" => catch = true,
            other => {
                return Err(HttpError::new(
                    500,
                    format!("continuation {cont} has unknown entry {other:?}"),
                ))
            }
        }
    }
    validate_continuation_shape(
        cont,
        input.is_some(),
        map.is_some(),
        run.is_some(),
        request.is_some(),
        eval.is_some(),
        then.is_some(),
        catch,
    )?;

    // The middle step, if any: `map` fans out over `in`'s children and yields a
    // `children` tree; `run` builds one request from its image + `in`; `request`
    // runs an already-complete ArgTree unchanged.
    let mid: Option<(gix::objs::tree::Entry, String, bool)> = if let Some(img) = &map {
        let input = input.as_ref().expect("validated input");
        // Map the children in parallel — one thread per child, each a full
        // [`run_work_request`] (so a child may itself promise). Concurrency is bounded by
        // the runner pool, not the thread count; threads are cheap and mostly
        // blocked. A blob `in` is a leaf: nothing to map, an empty children tree.
        let children: Vec<gix::objs::tree::Entry> = if input.mode.is_tree() {
            let kids = fetch_tree(config, &input.oid.to_string())
                .map_err(|e| HttpError::new(500, format!("reading map source: {e}")))?;
            let results: Vec<Result<gix::objs::tree::Entry, HttpError>> =
                std::thread::scope(|scope| {
                    let handles: Vec<_> = kids
                        .iter()
                        .map(|kid| {
                            let img = img.as_str();
                            scope.spawn(move || {
                                let arg = named_entry("in", kid.mode, kid.oid);
                                let result = run_image(
                                    config,
                                    img,
                                    vec![arg],
                                    salt,
                                    stack,
                                    secrets,
                                    // The map entry's own name — which for the
                                    // test suite is the test name, and is the
                                    // only thing that gives a 29-way fan-out
                                    // readable nodes. A `help` lookup cannot:
                                    // children are curried from `base`, not
                                    // from the tool's own ArgTree, so the
                                    // tool's help is not in their lineage.
                                    |arg_tree| {
                                        crate::status::child(
                                            config, parent, "map", &kid.name, arg_tree,
                                        )
                                    },
                                )?;
                                result_entry(&kid.name, &result)
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join().unwrap_or_else(|_| {
                                Err(HttpError::new(500, "a map worker thread panicked"))
                            })
                        })
                        .collect()
                });
            // Every child ran to completion (or failure) before we got here; the
            // first failure fails the whole map, exactly like a failing child in
            // the old blocking recursion.
            results.into_iter().collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        let children_tree = store_git_tree(config, children)
            .map_err(|e| HttpError::new(500, format!("storing children tree: {e}")))?;
        Some((
            named_entry("children", EntryKind::Tree.into(), children_tree),
            format!("tree {children_tree}"),
            false,
        ))
    } else if let Some(img) = &run {
        let input = input.as_ref().expect("validated input");
        // The single-valued form: `run(--in=<in>)`, fully resolved by [`run_work_request`]
        // (so a promise R leaves behind is already collapsed to a value here).
        Some(continuation_result(
            config,
            cont,
            run_image(
                config,
                img,
                vec![input.clone()],
                salt,
                stack,
                secrets,
                |arg_tree| crate::status::child(config, parent, "run", "run", arg_tree),
            ),
            catch,
        )?)
    } else if let Some(arg_tree) = &request {
        // The one form whose child needs no currying: the continuation carries
        // a complete ArgTree, so it is recorded as-is.
        crate::status::child(config, parent, "request", "request", arg_tree);
        Some(continuation_result(
            config,
            cont,
            run_work_request(
                config,
                &WorkRequest {
                    arg_tree,
                    stack,
                    secrets,
                },
            ),
            catch,
        )?)
    } else if let Some(path) = &eval {
        let input = input.as_ref().expect("validated input");
        // Walk `.caos-expr` from `in`'s root down to `path`, on THIS request
        // thread — its own `run`s dispatch through `run_image`, so cycle
        // detection, memoization and secret grants all ride the same `stack`/
        // `secrets`, and the object built is byte-identical to a client
        // `eval-path`. `catch` turns a failed walk into `--error`, like `run`.
        let host = ServerEvalHost {
            config,
            salt,
            stack,
            secrets,
            parent,
            eval_path: path,
            dispatches: std::sync::atomic::AtomicUsize::new(0),
        };
        let evaluated = caos_eval::eval_path(&host, &input.oid.to_string(), path)
            .map(|(kind, hash)| format!("{kind} {hash}"))
            .map_err(|e| {
                HttpError::new(
                    500,
                    format!("eval-path {path:?} in continuation {cont}: {e}"),
                )
            });
        Some(continuation_result(config, cont, evaluated, catch)?)
    } else {
        None
    };

    let caught = mid.as_ref().is_some_and(|(_, _, caught)| *caught);

    // The promise type, for the record. A continuation with only a `then` has
    // no middle step at all (a plain tail call), which is a shape in its own
    // right and not a missing one.
    let kind = match (&map, &run, &request, &eval) {
        (Some(_), _, _, _) => "map",
        (_, Some(_), _, _) => "run",
        (_, _, Some(_), _) => "request",
        (_, _, _, Some(_)) => "eval",
        _ => "then",
    };

    match (then, mid) {
        // `then` combines: it gets the original `in` when this is an image
        // continuation, plus the middle step's contribution when one ran. An
        // exact-request continuation has no `in`, so it passes only `result`
        // or `error`.
        (Some(img), mid) => {
            let mut args = Vec::new();
            if let Some(input) = input {
                args.push(input);
            }
            if let Some((extra, _, _)) = mid {
                args.push(extra);
            }
            // THE HANDLER INHERITS THE PARENT'S RUNNER POOL, and this is the
            // only place in the file that does it — the `run` and `map` sites
            // above deliberately do not.
            //
            // A continuation is the SAME LOGICAL WORK one stage on, which the
            // trace already models (`status::walk` follows a node's completion
            // as that node continuing, not as a child of it). So a job that
            // asked for a pool is still asking for it at its next stage, and
            // that has to hold without every worker remembering to re-bind it:
            // a test whose `next()` forgot dropped its later stages into the
            // general pool, freed its slot the moment it recorded a
            // continuation, and let another test start — so the cap bounded
            // the first job of each test and nothing else.
            //
            // A CHILD IS NOT THE SAME WORK. It is new work the parent may be
            // waiting for, so inheriting there would put a blocked parent and
            // the child it needs in one bounded pool, which is the deadlock the
            // pools exist to prevent.
            args.extend(inherited_pool_entries(config, parent)?);
            Ok((
                run_image(
                    config,
                    &img,
                    args,
                    salt,
                    stack,
                    secrets,
                    // The handler's ArgTree is the one thing in a continuation
                    // that is NOT derivable: its extra arg is the middle step's
                    // RESULT (the `children` tree, `--result`, or a caught
                    // `--error`), which no amount of replaying the continuation
                    // can produce.
                    |arg_tree| crate::status::continuation(config, parent, kind, Some(arg_tree)),
                )?,
                caught,
            ))
        }
        // No `then`: the middle step's own result is the request's result.
        (None, Some((_, result, _))) => {
            crate::status::continuation(config, parent, kind, None);
            Ok((result, caught))
        }
        // Unreachable — the presence check above requires some step.
        (None, None) => Err(HttpError::new(
            500,
            format!("continuation {cont} has no step to run"),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_continuation_shape(
    cont: &str,
    has_input: bool,
    has_map: bool,
    has_run: bool,
    has_request: bool,
    has_eval: bool,
    has_then: bool,
    catch: bool,
) -> Result<(), HttpError> {
    let middle_count = usize::from(has_map)
        + usize::from(has_run)
        + usize::from(has_request)
        + usize::from(has_eval);
    if middle_count > 1 {
        return Err(HttpError::new(
            500,
            format!(
                "continuation {cont} has more than one of 'map', 'run', 'request', and 'eval' (they are mutually exclusive)"
            ),
        ));
    }
    if has_request && has_input {
        return Err(HttpError::new(
            500,
            format!("continuation {cont} has both 'request' and 'in'"),
        ));
    }
    if !has_request && !has_input {
        return Err(HttpError::new(
            500,
            format!("continuation {cont} missing 'in'"),
        ));
    }
    if middle_count == 0 && !has_then {
        return Err(HttpError::new(
            500,
            format!("continuation {cont} has none of 'map', 'run', 'request', 'eval', or 'then'"),
        ));
    }
    // Both checked here rather than client-side only: a continuation is a tree
    // any worker can hand us, so the interpreter states its own contract.
    if catch && !has_run && !has_request && !has_eval {
        return Err(HttpError::new(
            500,
            format!(
                "continuation {cont} has 'catch' without 'run', 'request' or 'eval' (catch covers one fallible step)"
            ),
        ));
    }
    if catch && !has_then {
        return Err(HttpError::new(
            500,
            format!(
                "continuation {cont} has 'catch' without 'then' (nothing would receive the error)"
            ),
        ));
    }
    Ok(())
}

/// Turn one single-valued middle step into the callback entry/result pair,
/// optionally representing a failure as an `error` blob. Shared by rebuilt
/// `run` requests and exact `request` continuations so catch semantics cannot
/// drift between them.
/// The `required*` entries of `parent`, to be merged into the ArgTree of a
/// continuation OF that parent (see the call site in `resolve_promise`).
///
/// Read off the parent's own ArgTree rather than threaded through as a
/// parameter, because that is where the answer already is: `required-pool` is
/// an ordinary arg, and a job that has one has it in its entries.
///
/// Returns every `required*` entry, not `required-pool` specifically. The prefix
/// is the reserved namespace for runner selection (`caos_world`), so a second
/// pool dimension added later inherits without touching this.
fn inherited_pool_entries(
    config: &Config,
    parent: &str,
) -> Result<Vec<gix::objs::tree::Entry>, HttpError> {
    let entries = args_entries(config, parent)?;

    entries
        .into_iter()
        .filter(|(name, _)| name.starts_with(caos_world::REQUIRED_ARG_PREFIX))
        .map(|(name, oid)| {
            let oid = gix::ObjectId::from_hex(oid.as_bytes())
                .map_err(|e| HttpError::new(500, format!("invalid {name} oid: {e}")))?;
            Ok(named_entry(
                &name,
                gix::objs::tree::EntryKind::Blob.into(),
                oid,
            ))
        })
        .collect()
}

fn continuation_result(
    config: &Config,
    cont: &str,
    result: Result<String, HttpError>,
    catch: bool,
) -> Result<(gix::objs::tree::Entry, String, bool), HttpError> {
    use gix::objs::tree::EntryKind;

    match result {
        Ok(result) => Ok((result_entry("result", &result)?, result, false)),
        Err(error) if catch => {
            let text = error.message().to_string();
            eprintln!("caught sub-run failure in continuation {cont}: {text}");
            let oid = store_git_blob(config, text.as_bytes())
                .map_err(|e| HttpError::new(500, format!("storing error blob: {e}")))?;
            Ok((
                named_entry("error", EntryKind::Blob.into(), oid),
                format!("blob {oid}"),
                true,
            ))
        }
        Err(error) => Err(error),
    }
}

/// Run image `image_ref` over the given call args as a promise sub-run: unwrap
/// any curry layers and build the ArgTree — worker image folded in under its
/// reserved `base` entry, salt under `salt`, and std under `std` — whose hash IS
/// the request, built server-side byte-identically to what a client would build,
/// so the ArgTree hash (and cache key) is the same no matter who assembles it —
/// and send it through [`run_work_request`]. Returns `"<type> <hash>"`.
#[allow(clippy::too_many_arguments)] // the run context travels together
/// `record` sees the ArgTree this call FORMED, before it is run.
///
/// The tree exists only here: a worker hands over an image (`--map:hash=…`,
/// `--then:hash=…`) and the server curries the input into it, so nothing on
/// disk holds the identity of the child that actually ran. A reader given only
/// the continuation would have to replay this currying to name it, which is a
/// second implementation of key-forming logic whose drift fails silently — it
/// would point at hashes that have no records and report that nothing ran. So
/// the caller records what it ran, and the reader never curries at all.
fn run_image(
    config: &Config,
    image_ref: &str,
    call_args: Vec<gix::objs::tree::Entry>,
    salt: &str,
    stack: &[String],
    secrets: &[crate::secrets::Grant],
    record: impl FnOnce(&str),
) -> Result<String, HttpError> {
    use gix::objs::tree::EntryKind;

    let (image, bound) = unwrap_curry(config, image_ref)?;
    let store_err = |e: String| HttpError::new(500, format!("building sub-request: {e}"));

    // The worker image rides *in* the ArgTree under the reserved `base` entry
    // (embedded as the image's own tree for a git image, a ref blob for
    // `docker://`) — the same shape the client builds, merged last so the
    // reserved name wins over any like-named user arg.
    let image_entry = if image.len() == 40 && image.bytes().all(|b| b.is_ascii_hexdigit()) {
        let oid = gix::ObjectId::from_hex(image.as_bytes())
            .map_err(|e| HttpError::new(500, format!("invalid image hash: {e}")))?;
        named_entry("base", EntryKind::Tree.into(), oid)
    } else {
        named_entry(
            "base",
            EntryKind::Blob.into(),
            store_git_blob(config, image.as_bytes()).map_err(store_err)?,
        )
    };
    let mut args = merge_entries(merge_entries(bound, call_args), vec![image_entry]);
    // The salt also rides in the ArgTree, under its reserved entry — added (only
    // when non-empty) exactly as the client does, so the ArgTree, and hence the
    // request, is byte-identical. It is threaded down from the parent run.
    if !salt.is_empty() {
        let salt_entry = named_entry(
            "salt",
            EntryKind::Blob.into(),
            store_git_blob(config, salt.as_bytes()).map_err(store_err)?,
        );
        args = merge_entries(args, vec![salt_entry]);
    }
    // Cache-isolation tag (design/secrets.md): fold in `secret-hash` when this
    // sub-run is granted a secret — matched against the base entries, so two
    // callers with different secrets don't collide in the cache. Matched before
    // the entry is added (readers never pin it, so there's no self-reference),
    // and byte-identical to what the client folds into an equivalent top-level
    // ArgTree, so a computation shares one cache entry however it's reached.
    let base: std::collections::BTreeMap<String, String> = args
        .iter()
        .map(|e| {
            (
                String::from_utf8_lossy(e.filename.as_ref()).into_owned(),
                e.oid.to_string(),
            )
        })
        .collect();
    if let Some(hash) = crate::secrets::secret_hash(secrets, &base) {
        let entry = named_entry(
            caos_world::SECRET_HASH_ARG,
            EntryKind::Blob.into(),
            store_git_blob(config, hash.as_bytes()).map_err(store_err)?,
        );
        args = merge_entries(args, vec![entry]);
    }
    // The ArgTree IS the request — its hash is the cache key, nothing wraps it.
    let arg_tree = store_git_tree(config, args).map_err(store_err)?.to_string();
    record(&arg_tree);
    run_work_request(
        config,
        &WorkRequest {
            arg_tree: &arg_tree,
            stack,
            secrets,
        },
    )
}

/// Peel any curry layers off `image_ref`, returning the underlying plain image
/// and the args bound into it (outer layers win). The server-side counterpart of
/// the client's `unwrap_curry`, reading straight from the object database. A
/// hash that isn't a curry node (a git image, or any other object) passes
/// through unchanged.
fn unwrap_curry(
    config: &Config,
    image_ref: &str,
) -> Result<(String, Vec<gix::objs::tree::Entry>), HttpError> {
    let mut image = image_ref.to_string();
    let mut bound: Vec<gix::objs::tree::Entry> = Vec::new();
    while image.len() == 40 && image.bytes().all(|b| b.is_ascii_hexdigit()) {
        // Not a tree at all → not a curry node; let image resolution complain if
        // it isn't an image either.
        let Ok(entries) = fetch_tree(config, &image) else {
            break;
        };
        if !entries.iter().any(|e| e.name == CURRY_MARKER) {
            break;
        }
        let find = |name: &str| {
            entries
                .iter()
                .find(|e| e.name == name)
                .map(|e| e.oid.to_string())
                .ok_or_else(|| HttpError::new(500, format!("curry node {image} missing {name:?}")))
        };
        let base = blob_string(config, &find("base")?)?;
        let args = fetch_tree(config, &find("args")?)
            .map_err(|e| HttpError::new(500, format!("curry node {image} args: {e}")))?
            .into_iter()
            .map(|e| named_entry(&e.name, e.mode, e.oid))
            .collect();
        // `bound` holds outer layers, which win over this deeper one.
        bound = merge_entries(args, bound);
        image = base;
    }
    Ok((image, bound))
}

/// Merge two sets of tree entries by filename; entries in `high` override those
/// in `low`. Order is irrelevant — `store_tree` sorts before encoding.
fn merge_entries(
    low: Vec<gix::objs::tree::Entry>,
    high: Vec<gix::objs::tree::Entry>,
) -> Vec<gix::objs::tree::Entry> {
    let mut by_name = std::collections::BTreeMap::new();
    for e in low.into_iter().chain(high) {
        by_name.insert(e.filename.to_vec(), e);
    }
    by_name.into_values().collect()
}

/// A gix tree entry with the given name.
fn named_entry(
    name: &str,
    mode: gix::objs::tree::EntryMode,
    oid: gix::ObjectId,
) -> gix::objs::tree::Entry {
    gix::objs::tree::Entry {
        mode,
        filename: name.as_bytes().to_vec().into(),
        oid,
    }
}

/// Turn a sub-run result `"<type> <hash>"` into a tree entry named `name`. A
/// `commit` result rides as a gitlink entry (mode 160000) — workers fetch
/// objects by hash explicitly, so git's don't-fetch gitlink semantics never
/// apply inside caos.
fn result_entry(name: &str, result: &str) -> Result<gix::objs::tree::Entry, HttpError> {
    use gix::objs::tree::EntryKind;
    let (kind, hash) = result
        .split_once(' ')
        .ok_or_else(|| HttpError::new(500, format!("malformed sub-run result: {result:?}")))?;
    let mode = match kind {
        "tree" => EntryKind::Tree,
        "blob" => EntryKind::Blob,
        "commit" => EntryKind::Commit,
        other => {
            return Err(HttpError::new(
                500,
                format!("sub-run returned unexpected type {other:?}"),
            ))
        }
    };
    let oid = gix::ObjectId::from_hex(hash.trim().as_bytes())
        .map_err(|e| HttpError::new(500, format!("sub-run returned invalid hash: {e}")))?;
    Ok(named_entry(name, mode.into(), oid))
}

/// The ArgTree's top-level name → oid map — what a runner's required args are
/// matched against (pure oid equality; see `crate::runner`).
fn args_entries(
    config: &Config,
    arg_tree: &str,
) -> Result<std::collections::BTreeMap<String, String>, HttpError> {
    let entries = fetch_tree(config, arg_tree)
        .map_err(|e| HttpError::new(400, format!("reading arg tree: {e}")))?;
    Ok(entries
        .into_iter()
        .map(|e| (e.name, e.oid.to_string()))
        .collect())
}

/// Unpack an ArgTree into the reserved entries the server needs: the image ref
/// (its `base` entry), the std-tree hash (its `std` entry, empty if none), and
/// the salt (its `salt` entry, empty if none). `base`/`std`/`salt` are all
/// entries of this one tree, so the ArgTree's hash *is* the cache key with
/// nothing keyed alongside it — the ArgTree hash itself is the request identity,
/// so it is not returned here.
fn read_arg_tree(config: &Config, arg_tree: &str) -> Result<(String, String), HttpError> {
    let entries = fetch_tree(config, arg_tree)
        .map_err(|e| HttpError::new(400, format!("reading arg tree: {e}")))?;
    let mut image = None;
    let mut salt = String::new();
    for entry in entries {
        match entry.name.as_str() {
            // A git-docker image *is* a git tree, so it rides embedded (the entry
            // is a tree, its oid the image hash — the image travels inside the
            // ArgTree graph); a `docker://` image has no git object, so it rides
            // as a blob naming the registry ref.
            "base" => {
                image = Some(if entry.mode.is_tree() {
                    entry.oid.to_string()
                } else {
                    blob_string(config, &entry.oid.to_string())?
                });
            }
            // std and salt are plain blobs (std NAMES the std tree; salt is opaque).
            "salt" => salt = blob_string(config, &entry.oid.to_string())?,
            _ => {}
        }
    }
    let image = image.ok_or_else(|| HttpError::new(400, "arg tree missing 'base'"))?;
    Ok((image, salt))
}

/// Fetch a blob and return its content as a trimmed string.
fn blob_string(config: &Config, hash: &str) -> Result<String, HttpError> {
    let bytes =
        fetch_blob(config, hash).map_err(|e| HttpError::new(400, format!("reading blob: {e}")))?;
    String::from_utf8(bytes)
        .map(|s| s.trim().to_string())
        .map_err(|e| HttpError::new(400, format!("blob {hash} not UTF-8: {e}")))
}

/// The hash in a `"<type> <hash>"` result string (empty if malformed).
fn result_hash(result: &str) -> &str {
    result.split_whitespace().nth(1).unwrap_or("")
}

/// Pin `refs/caos/res/<argTreeHash>` at the result so a client can fetch it by
/// ref and it survives gc. This is part of top-level run success, particularly
/// when the HTTP caller disconnected before the eventual result.
fn pin_result(config: &Config, arg_tree: &str, result: &str) -> Result<(), HttpError> {
    let hash = result_hash(result);
    if hash.is_empty() {
        return Err(HttpError::new(
            500,
            format!("cannot pin malformed result {result:?}"),
        ));
    }
    let refname = format!("refs/caos/res/{arg_tree}");
    pin_result_in(&config.git_dir, &refname, hash)
}

fn pin_result_in(git_dir: &str, refname: &str, hash: &str) -> Result<(), HttpError> {
    const ATTEMPTS: usize = 8;
    let mut last_error = String::new();
    for attempt in 0..ATTEMPTS {
        match Command::new("git")
            .args(["-C", git_dir, "update-ref", refname, hash])
            .output()
        {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                last_error = format!(
                    "git update-ref exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Err(error) => last_error = format!("running git update-ref: {error}"),
        }
        if attempt + 1 < ATTEMPTS {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    Err(HttpError::new(
        500,
        format!("pinning {refname} after {ATTEMPTS} attempts: {last_error}"),
    ))
}

/// Require an immutable Docker distribution reference. `name:tag@digest` is
/// accepted because Docker resolves by the digest; a bare tag or digest-like
/// image id is not a registry locator pinned by the manifest digest required by
/// the spec.
fn validate_docker_reference(reference: &str, allow_seeded: bool) -> Result<(), String> {
    if allow_seeded && (reference == "seeded" || reference.starts_with("seeded-")) {
        return Ok(());
    }
    if reference.is_empty() || reference.starts_with('-') {
        return Err(format!("invalid docker image: {reference:?}"));
    }
    let Some((name, digest)) = reference.rsplit_once('@') else {
        return Err(format!(
            "docker image {reference:?} is mutable; use <name>@sha256:<64 lowercase hex digits>"
        ));
    };
    let Some(encoded) = digest.strip_prefix("sha256:") else {
        return Err(format!(
            "docker image {reference:?} is not pinned by a sha256 digest"
        ));
    };
    if name.is_empty()
        || encoded.len() != 64
        || !encoded
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return Err(format!(
            "invalid docker digest reference {reference:?}; expected <name>@sha256:<64 lowercase hex digits>"
        ));
    }
    Ok(())
}

/// Resolve the `image` parameter to a reference the host docker daemon can run.
///
/// `docker://<ref>` is a digest-pinned docker reference, used as-is. The only
/// non-digest exceptions are the internal `seeded*` rendezvous sentinels, which
/// are answered by seed runners and never pulled. Anything else is one of our
/// git images (the default): convert it to a real image, push it to the registry,
/// and return a digest reference into the registry.
fn resolve_image(config: &Config, image: &str) -> Result<String, HttpError> {
    if let Some(reference) = image.strip_prefix(DOCKER_SCHEME) {
        validate_docker_reference(reference, true).map_err(|e| HttpError::new(400, e))?;
        return Ok(reference.to_string());
    }
    if !image.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HttpError::new(
            400,
            format!("git image must be a hex hash (or use {DOCKER_SCHEME}<ref>): {image:?}"),
        ));
    }
    // The nested test stack (tests/lib/run-test.sh) runs on images the
    // outer suite already built and pushed, so its server passes git images
    // through unconverted: no OCI convert, no registry round-trip. The
    // default keeps converting.
    if std::env::var("CAOS_IMAGE_RESOLVE").as_deref() == Ok("none") {
        return Ok(image.to_string());
    }
    // A git image tree, converted. NOT a flake: do not add a branch here that
    // notices `flake.nix` + `flake.lock` and builds it. Doing so needs a builder
    // resolved BY NAME out of an ambient library, which is the one thing the
    // server must not have — it holds an arg tree, not a project tree, so it
    // cannot resolve a dependency by descent the way a client can. A flake
    // directory says `run --base:@=DEEP-DEPS/flake-builder --in:@=.` and the CLIENT
    // evaluates it, so what arrives here is already an image (design/caos-expr.md).
    convert_git_image(config, image)
        .map_err(|e| HttpError::new(500, format!("converting git image {image}: {e}")))
}

/// The lock for one cache key, minted on first use. A redis cache read followed
/// by a redis cache write is CHECK-THEN-ACT, and this server answers requests on
/// a thread pool: when a suite fans out, every client asks for the same image at
/// the same moment, every one of them misses, and every one of them does the
/// whole job. Measured: 29 tests, 29 identical `converted image` lines, each
/// materializing a ~200 MB tree to a temp dir, tarring it, sha256ing it and
/// pushing it to the registry. That was ~24s in which the suite created no
/// containers at all, and it is the skopeo/gzip CPU a `top` during a run shows.
///
/// One process per stack, so an in-process lock is the whole requirement — no
/// redis lock, no lease, nothing to expire. The waiters RE-READ the cache after
/// acquiring, so the winner's result is what they all return.
///
/// The map is never pruned. It holds one small entry per distinct image and
/// layer this server has ever converted, which is bounded by the work it has
/// done and is kilobytes at the scale that matters.
fn key_lock(key: &str) -> std::sync::Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, std::sync::Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = locks.lock().unwrap_or_else(|e| e.into_inner());
    std::sync::Arc::clone(
        map.entry(key.to_string())
            .or_insert_with(|| std::sync::Arc::new(Mutex::new(()))),
    )
}

/// Convert the git-docker image tree `git_hash` to a real image and push it to
/// the registry, returning a digest reference. Cached in Redis by git hash, and
/// SINGLE-FLIGHTED behind [`key_lock`] — see there for why the cache alone is
/// not enough.
fn convert_git_image(config: &Config, git_hash: &str) -> Result<String, String> {
    let image_key = format!("caos:image:{git_hash}");
    if let Ok(Some(manifest_digest)) = cache_get(&config.redis_addr, &image_key) {
        eprintln!("image cache hit: {git_hash} -> {manifest_digest}");
        return Ok(image_ref(config, &manifest_digest));
    }
    let lock = key_lock(&image_key);
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    // RE-READ under the lock: while we waited, the winner finished and cached.
    if let Ok(Some(manifest_digest)) = cache_get(&config.redis_addr, &image_key) {
        eprintln!("image cache hit (after wait): {git_hash} -> {manifest_digest}");
        return Ok(image_ref(config, &manifest_digest));
    }

    // The image tree holds `config.json` (a blob), `layer<NN>` subtrees, and an
    // optional `base` blob naming a `docker://<ref>` to stack our layers on top
    // of — so a heavy toolchain rides as registry layers pulled from its source,
    // never as git objects.
    let mut config_oid: Option<String> = None;
    let mut base_oid: Option<String> = None;
    let mut layers: Vec<(u64, String)> = Vec::new();
    for entry in fetch_tree(config, git_hash)? {
        if entry.name == "config.json" {
            config_oid = Some(entry.oid.to_string());
        } else if entry.name == "base" {
            base_oid = Some(entry.oid.to_string());
        } else if let Some(suffix) = entry.name.strip_prefix("layer") {
            // layer<NN>: number it for ordering (matches config.rootfs.diff_ids).
            if let Ok(num) = suffix.parse::<u64>() {
                if !entry.mode.is_tree() {
                    return Err(format!("layer entry {} is not a directory", entry.name));
                }
                layers.push((num, entry.oid.to_string()));
            }
        }
    }
    let config_oid = config_oid.ok_or("image tree has no config.json")?;
    let has_base = base_oid.is_some();
    if !has_base && layers.is_empty() {
        return Err("image tree has no base and no layer<NN> entries".to_string());
    }
    layers.sort_by_key(|(num, _)| *num);

    // A manifest layer is (mediaType, digest, size); a diff_id is the layer's
    // *uncompressed* sha256. A `base`'s layers and diff_ids come from the copied
    // base image (its layers are usually gzipped, so digest != diff_id). Our own
    // layers are uncompressed tar, so digest == diff_id. Base layers go on the
    // bottom; ours stack on top.
    let mut manifest_layers: Vec<(String, String, u64)> = Vec::new();
    let mut diff_ids: Vec<String> = Vec::new();
    if let Some(base_oid) = base_oid {
        let base_ref = String::from_utf8(fetch_blob(config, &base_oid)?)
            .map_err(|e| format!("base ref not UTF-8: {e}"))?;
        let base_ref = base_ref.trim();
        let base_ref = base_ref.strip_prefix(DOCKER_SCHEME).unwrap_or(base_ref);
        validate_docker_reference(base_ref, false)?;
        let (base_layers, base_diff_ids) = fetch_base(config, base_ref)?;
        diff_ids.extend(base_diff_ids);
        manifest_layers.extend(base_layers);
    }
    for (_, oid) in &layers {
        let (digest, size) = ensure_layer(config, oid)?;
        diff_ids.push(digest.clone());
        manifest_layers.push((OCI_LAYER_MEDIA_TYPE.to_string(), digest, size));
    }

    // Set the config's diff_ids to the full stack (base ++ ours) so the image is
    // self-consistent. We generate them outright — the stored config needn't
    // carry diff_ids (the producer can't know them without tarring / resolving
    // the base).
    let config_bytes = fetch_blob(config, &config_oid)?;
    let new_config = set_config_diff_ids(&config_bytes, &diff_ids)?;
    let config_digest = format!("sha256:{}", sha256_hex(&new_config));
    push_blob(config, &config_digest, &new_config)?;

    let manifest = build_manifest(&config_digest, new_config.len() as u64, &manifest_layers);
    let manifest_bytes =
        serde_json::to_vec(&manifest).map_err(|e| format!("serializing manifest: {e}"))?;
    let manifest_digest = format!("sha256:{}", sha256_hex(&manifest_bytes));
    push_manifest(config, &manifest_digest, &manifest_bytes)?;

    let _ = cache_set(&config.redis_addr, &image_key, &manifest_digest);
    eprintln!("converted image {git_hash} -> {manifest_digest}");
    Ok(image_ref(config, &manifest_digest))
}

/// The digest reference the host daemon uses to pull the converted image.
fn image_ref(config: &Config, manifest_digest: &str) -> String {
    format!(
        "{}/{REGISTRY_REPO}@{manifest_digest}",
        config.registry_pull_host.trim_end_matches('/')
    )
}

/// Copy a base image (`base_ref`, a bare docker reference) from its source
/// registry into our own repo with skopeo, so its blobs are available for a
/// converted git image to reference. Returns the base's manifest layers
/// `(media_type, digest, size)` and its config `diff_id`s (uncompressed digests)
/// — the lower part of the stack our delta layers sit on. `--format oci` rewrites
/// the manifest to OCI media types so it composes cleanly with our OCI layers;
/// the layer *blobs* (and their digests) are untouched.
fn fetch_base(config: &Config, base_ref: &str) -> Result<BaseLayers, String> {
    let push = config.registry_push_url.trim_end_matches('/');
    let host = push
        .strip_prefix("http://")
        .or_else(|| push.strip_prefix("https://"))
        .unwrap_or(push);
    // A deterministic tag per base ref: re-converting reuses the same copy.
    let tag = format!("base-{}", sha256_hex(base_ref.as_bytes()));
    let dest = format!("docker://{host}/{REGISTRY_REPO}:{tag}");
    let man_url = format!("{push}/v2/{REGISTRY_REPO}/manifests/{tag}");
    let accept = "application/vnd.oci.image.manifest.v1+json, \
                  application/vnd.docker.distribution.manifest.v2+json";

    // Skip the (slow, network-bound) skopeo pull if this base is already in the
    // registry from an earlier convert — the tag is deterministic per ref, so a
    // resolvable manifest means the blobs are present. This makes the stock base a
    // once-per-registry cost, not once-per-convert.
    let cached = minreq::get(&man_url)
        .with_header("Accept", accept)
        .send()
        .map(|r| (200..300).contains(&r.status_code))
        .unwrap_or(false);
    if !cached {
        let status = Command::new("skopeo")
            .args([
                "--insecure-policy",
                "copy",
                "--format",
                "oci",
                "--src-tls-verify=false",
                "--dest-tls-verify=false",
                "--override-os",
                "linux",
                "--override-arch",
                "amd64",
            ])
            .arg(format!("docker://{base_ref}"))
            .arg(&dest)
            // The slim server image runs as uid 0 with no /etc/passwd entry, so
            // skopeo can't resolve $HOME (it wants one for its auth/config dirs).
            // Point it at a writable dir so the anonymous pull works.
            .env("HOME", "/tmp")
            .status()
            .map_err(|e| format!("skopeo copy {base_ref}: {e}"))?;
        if !status.success() {
            return Err(format!(
                "skopeo copy {base_ref} -> {dest} failed ({status})"
            ));
        }
    }

    // Read the manifest (just copied, or already cached): the base layers' media
    // types/digests/sizes.
    let resp = minreq::get(&man_url)
        .with_header("Accept", accept)
        .send()
        .map_err(|e| format!("GET {man_url}: {e}"))?;
    if !(200..300).contains(&resp.status_code) {
        return Err(format!(
            "reading base manifest {tag}: {} {}",
            resp.status_code, resp.reason_phrase
        ));
    }
    let manifest: serde_json::Value = serde_json::from_slice(resp.as_bytes())
        .map_err(|e| format!("parsing base manifest: {e}"))?;
    let layers = manifest["layers"]
        .as_array()
        .ok_or("base manifest has no layers")?
        .iter()
        .map(|l| {
            let media = l["mediaType"]
                .as_str()
                .unwrap_or(OCI_LAYER_MEDIA_TYPE)
                .to_string();
            let digest = l["digest"].as_str().unwrap_or_default().to_string();
            let size = l["size"].as_u64().unwrap_or_default();
            (media, digest, size)
        })
        .collect::<Vec<_>>();
    let config_digest = manifest["config"]["digest"]
        .as_str()
        .ok_or("base manifest has no config digest")?;

    // Read the base config blob for its uncompressed diff_ids.
    let cfg_url = format!("{push}/v2/{REGISTRY_REPO}/blobs/{config_digest}");
    let resp = minreq::get(&cfg_url)
        .send()
        .map_err(|e| format!("GET {cfg_url}: {e}"))?;
    if !(200..300).contains(&resp.status_code) {
        return Err(format!(
            "reading base config {config_digest}: {} {}",
            resp.status_code, resp.reason_phrase
        ));
    }
    let cfg: serde_json::Value =
        serde_json::from_slice(resp.as_bytes()).map_err(|e| format!("parsing base config: {e}"))?;
    let diff_ids = cfg["rootfs"]["diff_ids"]
        .as_array()
        .ok_or("base config has no rootfs.diff_ids")?
        .iter()
        .map(|d| d.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    if layers.len() != diff_ids.len() {
        return Err(format!(
            "base layer/diff_id count mismatch: {} layers vs {} diff_ids",
            layers.len(),
            diff_ids.len()
        ));
    }
    Ok((layers, diff_ids))
}

/// Build (if not cached) and push the layer whose git tree is `layer_oid`,
/// returning its `(digest, size)`. The git-hash → digest+size mapping is cached
/// in Redis so an unchanged layer is never re-tarred or re-pushed.
fn ensure_layer(config: &Config, layer_oid: &str) -> Result<(String, u64), String> {
    let key = format!("caos:layer:{layer_oid}");
    if let Ok(Some(value)) = cache_get(&config.redis_addr, &key) {
        if let Some((digest, size)) = value.split_once(' ') {
            if let Ok(size) = size.parse::<u64>() {
                eprintln!("layer cache hit: {layer_oid} -> {digest}");
                return Ok((digest.to_string(), size));
            }
        }
    }
    // Single-flighted for the same reason as the image above, and this is where
    // the bytes actually are: build_layer_tar materializes the whole tree to a
    // temp dir and tars it, so N concurrent misses are N full copies of the
    // layer on disk at once as well as N pushes.
    let lock = key_lock(&key);
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(Some(value)) = cache_get(&config.redis_addr, &key) {
        if let Some((digest, size)) = value.split_once(' ') {
            if let Ok(size) = size.parse::<u64>() {
                eprintln!("layer cache hit (after wait): {layer_oid} -> {digest}");
                return Ok((digest.to_string(), size));
            }
        }
    }
    let tar = build_layer_tar(config, layer_oid)?;
    let digest = format!("sha256:{}", sha256_hex(&tar));
    let size = tar.len() as u64;
    push_blob(config, &digest, &tar)?;
    let _ = cache_set(&config.redis_addr, &key, &format!("{digest} {size}"));
    eprintln!("converted layer {layer_oid} -> {digest} ({size} bytes)");
    Ok((digest, size))
}

/// Materialize a layer's git tree to a temp dir, apply its `.caosmeta` sidecars,
/// and tar it deterministically (GNU format handles the long /nix/store paths and
/// symlinks; the flags zero the mtimes and sort entries, so the output — hence its
/// digest — is stable).
fn build_layer_tar(config: &Config, tree_hash: &str) -> Result<Vec<u8>, String> {
    let dir = temp_dir()?;
    let result = (|| {
        materialize_tree(config, &dir, tree_hash)?;
        apply_layer_metadata(&dir)?;
        tar_dir(&dir)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Apply the `<name>.caosmeta` sidecars written by `import-image`: for each one,
/// restore the sibling entry's mode and owner, then remove the sidecar so it
/// doesn't land in the layer tar. We run as root, so chmod/chown/unlink and the
/// later read-for-tar all work regardless of the perms we set.
fn apply_layer_metadata(dir: &Path) -> Result<(), String> {
    let mut sidecars = Vec::new();
    let mut subdirs = Vec::new();
    for dirent in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let dirent = dirent.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = dirent.file_name().to_string_lossy().into_owned();
        if let Some(target) = name.strip_suffix(META_SUFFIX) {
            sidecars.push((dirent.path(), dir.join(target)));
        } else if dirent
            .file_type()
            .map_err(|e| format!("{}: {e}", dirent.path().display()))?
            .is_dir()
        {
            subdirs.push(dirent.path());
        }
    }

    for (sidecar, target) in sidecars {
        let bytes = std::fs::read(&sidecar).map_err(|e| format!("{}: {e}", sidecar.display()))?;
        let meta: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", sidecar.display()))?;
        let mode = meta
            .get("mode")
            .and_then(|v| v.as_str())
            .and_then(|s| u32::from_str_radix(s, 8).ok())
            .ok_or_else(|| format!("{}: missing/invalid mode", sidecar.display()))?;
        let uid = meta.get("uid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let gid = meta.get("gid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        std::os::unix::fs::chown(&target, Some(uid), Some(gid))
            .map_err(|e| format!("chown {}: {e}", target.display()))?;
        set_mode(&target, mode)?;
        std::fs::remove_file(&sidecar).map_err(|e| format!("{}: {e}", sidecar.display()))?;
    }

    for subdir in subdirs {
        apply_layer_metadata(&subdir)?;
    }
    Ok(())
}

/// A fresh, unique temp directory.
fn temp_dir() -> Result<PathBuf, String> {
    let base = std::env::temp_dir().join("caos-convert");
    std::fs::create_dir_all(&base).map_err(|e| format!("creating {}: {e}", base.display()))?;
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!("{}-{n}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Write a git tree's contents into `dir`: files (with their exec bit), symlinks,
/// and subdirectories, recursively. Modes are set explicitly so the tar is
/// independent of the umask.
fn materialize_tree(config: &Config, dir: &Path, tree_hash: &str) -> Result<(), String> {
    use gix::objs::tree::EntryKind;
    for entry in fetch_tree(config, tree_hash)? {
        let path = dir.join(&entry.name);
        match entry.mode.kind() {
            EntryKind::Tree => {
                std::fs::create_dir(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                set_mode(&path, 0o755)?;
                materialize_tree(config, &path, &entry.oid.to_string())?;
            }
            EntryKind::Link => {
                let target = fetch_blob(config, &entry.oid.to_string())?;
                symlink(Path::new(std::ffi::OsStr::from_bytes(&target)), &path)
                    .map_err(|e| format!("symlink {}: {e}", path.display()))?;
            }
            EntryKind::Blob | EntryKind::BlobExecutable => {
                let content = fetch_blob(config, &entry.oid.to_string())?;
                std::fs::write(&path, content).map_err(|e| format!("{}: {e}", path.display()))?;
                let mode = if entry.mode.kind() == EntryKind::BlobExecutable {
                    0o755
                } else {
                    0o644
                };
                set_mode(&path, mode)?;
            }
            EntryKind::Commit => {
                return Err(format!("unexpected submodule entry: {}", entry.name));
            }
        }
    }
    Ok(())
}

/// Set a path's permission bits.
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {}: {e}", path.display()))
}

/// Tar `dir`'s contents reproducibly (GNU format, zeroed mtimes, sorted, numeric
/// owners read from disk — which the `.caosmeta` sidecars already set).
fn tar_dir(dir: &Path) -> Result<Vec<u8>, String> {
    let output = Command::new("tar")
        .args([
            "--format=gnu",
            "--numeric-owner",
            "--mtime=@0",
            "--sort=name",
        ])
        .arg("-C")
        .arg(dir)
        .args(["-cf", "-", "."])
        .output()
        .map_err(|e| format!("running tar: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "tar failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    Ok(output.stdout)
}

/// Set `rootfs.diff_ids` in the image config to `diff_ids` (in layer order),
/// creating `rootfs` if absent — we generate these outright rather than reading
/// any stored value, so the config needn't carry diff_ids (the producer can't
/// know them without tarring). Everything else in the config passes through;
/// other keys may be reordered by re-serialization, which is fine since we
/// compute the config digest from the result.
fn set_config_diff_ids(config_bytes: &[u8], diff_ids: &[String]) -> Result<Vec<u8>, String> {
    let mut value: serde_json::Value =
        serde_json::from_slice(config_bytes).map_err(|e| format!("parsing config.json: {e}"))?;
    let obj = value
        .as_object_mut()
        .ok_or("config.json is not a JSON object")?;
    let rootfs = obj.entry("rootfs").or_insert_with(|| serde_json::json!({}));
    let rootfs = rootfs
        .as_object_mut()
        .ok_or("config.json rootfs is not an object")?;
    rootfs.insert(
        "type".to_string(),
        serde_json::Value::String("layers".to_string()),
    );
    rootfs.insert(
        "diff_ids".to_string(),
        serde_json::Value::Array(
            diff_ids
                .iter()
                .map(|d| serde_json::Value::String(d.clone()))
                .collect(),
        ),
    );
    serde_json::to_vec(&value).map_err(|e| format!("serializing config.json: {e}"))
}

/// Build the OCI image manifest referencing the config and layer blobs.
fn build_manifest(
    config_digest: &str,
    config_size: u64,
    layers: &[(String, String, u64)],
) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_size,
        },
        "layers": layers.iter().map(|(media_type, digest, size)| serde_json::json!({
            "mediaType": media_type,
            "digest": digest,
            "size": size,
        })).collect::<Vec<_>>(),
    })
}

/// Upload a blob to the registry (monolithic two-step: start, then PUT bytes).
fn push_blob(config: &Config, digest: &str, data: &[u8]) -> Result<(), String> {
    let base = config.registry_push_url.trim_end_matches('/');
    let start = format!("{base}/v2/{REGISTRY_REPO}/blobs/uploads/");
    let response = minreq::post(&start)
        .send()
        .map_err(|e| format!("POST {start}: {e}"))?;
    if response.status_code != 202 {
        return Err(format!(
            "starting blob upload: {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    let location = response
        .headers
        .get("location")
        .ok_or("blob upload response missing Location")?
        .clone();
    let upload = if location.starts_with("http://") || location.starts_with("https://") {
        location
    } else {
        format!("{base}{location}")
    };
    let sep = if upload.contains('?') { '&' } else { '?' };
    let put = format!("{upload}{sep}digest={digest}");
    let response = minreq::put(&put)
        .with_header("Content-Type", "application/octet-stream")
        .with_body(data.to_vec())
        .send()
        .map_err(|e| format!("PUT {put}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "uploading blob {digest}: {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    Ok(())
}

/// Upload a manifest to the registry, addressed by its digest.
fn push_manifest(config: &Config, digest: &str, data: &[u8]) -> Result<(), String> {
    const ATTEMPTS: usize = 8;
    let base = config.registry_push_url.trim_end_matches('/');
    let url = format!("{base}/v2/{REGISTRY_REPO}/manifests/{digest}");
    for attempt in 0..ATTEMPTS {
        let response = minreq::put(&url)
            .with_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
            .with_body(data.to_vec())
            .send()
            .map_err(|e| format!("PUT {url}: {e}"))?;
        if (200..300).contains(&response.status_code) {
            return Ok(());
        }
        // Concurrent conversion of one cold image can race in distribution:
        // one request observes the shared layer while its content link is
        // still becoming visible, and the manifest PUT briefly reports
        // `manifest blob unknown` (400). The manifest is content-addressed, so
        // retrying this one response is idempotent. Other client errors are
        // permanent; server errors remain loud instead of being hidden here.
        if response.status_code != 400 || attempt + 1 == ATTEMPTS {
            return Err(format!(
                "uploading manifest {digest}: {} {}",
                response.status_code, response.reason_phrase
            ));
        }
        let delay_ms = 25_u64 << attempt.min(6);
        std::thread::sleep(Duration::from_millis(delay_ms));
    }
    unreachable!("manifest upload loop either succeeds or returns its last error")
}

/// Hex sha256 of `data`.
fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `GET key` from Redis, returning the value or None if the key is absent.
fn cache_get(addr: &str, key: &str) -> Result<Option<String>, String> {
    let mut stream = redis_connect(addr)?;
    stream
        .write_all(&resp_command(&["GET", key]))
        .map_err(|e| format!("write: {e}"))?;
    read_bulk_reply(&mut BufReader::new(stream))
}

/// `SET key value` in Redis.
fn cache_set(addr: &str, key: &str, value: &str) -> Result<(), String> {
    let mut stream = redis_connect(addr)?;
    stream
        .write_all(&resp_command(&["SET", key, value]))
        .map_err(|e| format!("write: {e}"))?;
    read_status_reply(&mut BufReader::new(stream))
}

/// `RPUSH key value` — append one element to a Redis list.
///
/// The trace record ([`crate::status`]) is a list rather than a rewritten blob
/// precisely so this is an APPEND: a read-modify-write of a growing JSON value
/// would be check-then-act across the threads a map fans out onto, and would
/// re-serialize the whole record once per child. Redis serializes concurrent
/// pushes to one key itself, so the map's threads need no coordination — and
/// since every event carries its own timestamp, list order is not load-bearing.
pub(crate) fn list_append(addr: &str, key: &str, value: &str) -> Result<(), String> {
    let mut stream = redis_connect(addr)?;
    stream
        .write_all(&resp_command(&["RPUSH", key, value]))
        .map_err(|e| format!("write: {e}"))?;
    read_integer_reply(&mut BufReader::new(stream)).map(|_| ())
}

/// `LRANGE key 0 -1` — the whole list, in insertion order.
pub(crate) fn list_read(addr: &str, key: &str) -> Result<Vec<String>, String> {
    let mut stream = redis_connect(addr)?;
    stream
        .write_all(&resp_command(&["LRANGE", key, "0", "-1"]))
        .map_err(|e| format!("write: {e}"))?;
    read_array_reply(&mut BufReader::new(stream))
}

/// Connect to Redis with read/write timeouts so a stuck server can't hang us.
fn redis_connect(addr: &str) -> Result<TcpStream, String> {
    let stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    let _ = stream.set_read_timeout(Some(REDIS_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REDIS_TIMEOUT));
    Ok(stream)
}

/// Encode a Redis command as a RESP array of bulk strings (binary-safe, so the
/// NUL in our cache key is fine).
fn resp_command(args: &[&str]) -> Vec<u8> {
    let mut buf = format!("*{}\r\n", args.len()).into_bytes();
    for arg in args {
        buf.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        buf.extend_from_slice(arg.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

/// Read a RESP bulk-string reply (`$<len>\r\n<data>\r\n`); a nil reply (`$-1`)
/// becomes None and an error reply (`-...`) becomes Err.
fn read_bulk_reply(reader: &mut impl BufRead) -> Result<Option<String>, String> {
    let header = read_reply_line(reader)?;
    match header.as_bytes().first() {
        Some(b'$') => {
            let len: i64 = header[1..]
                .parse()
                .map_err(|e| format!("bad bulk length: {e}"))?;
            if len < 0 {
                return Ok(None); // nil
            }
            let mut buf = vec![0u8; len as usize + 2]; // data + trailing CRLF
            reader
                .read_exact(&mut buf)
                .map_err(|e| format!("read: {e}"))?;
            buf.truncate(len as usize);
            String::from_utf8(buf)
                .map(Some)
                .map_err(|e| format!("non-utf8 value: {e}"))
        }
        Some(b'-') => Err(format!("redis error: {}", &header[1..])),
        _ => Err(format!("unexpected reply: {header:?}")),
    }
}

/// Read a RESP integer reply (`:<n>\r\n`); an error reply becomes Err.
fn read_integer_reply(reader: &mut impl BufRead) -> Result<i64, String> {
    let header = read_reply_line(reader)?;
    match header.as_bytes().first() {
        Some(b':') => header[1..]
            .parse()
            .map_err(|e| format!("bad integer reply: {e}")),
        Some(b'-') => Err(format!("redis error: {}", &header[1..])),
        _ => Err(format!("unexpected reply: {header:?}")),
    }
}

/// Read a RESP array reply (`*<n>\r\n` followed by `n` bulk strings). A nil
/// array (`*-1`) and an empty one both become an empty Vec — for a trace record
/// "no events" and "no such key" are the same answer, and neither is an error.
fn read_array_reply(reader: &mut impl BufRead) -> Result<Vec<String>, String> {
    let header = read_reply_line(reader)?;
    match header.as_bytes().first() {
        Some(b'*') => {
            let count: i64 = header[1..]
                .parse()
                .map_err(|e| format!("bad array length: {e}"))?;
            let mut out = Vec::new();
            for _ in 0..count.max(0) {
                match read_bulk_reply(reader)? {
                    Some(item) => out.push(item),
                    // A nil element inside a LRANGE is not a thing redis emits;
                    // treat it as the end rather than inventing a placeholder.
                    None => break,
                }
            }
            Ok(out)
        }
        Some(b'-') => Err(format!("redis error: {}", &header[1..])),
        _ => Err(format!("unexpected reply: {header:?}")),
    }
}

/// Read a RESP simple-status reply (`+OK\r\n`); an error reply becomes Err.
fn read_status_reply(reader: &mut impl BufRead) -> Result<(), String> {
    let header = read_reply_line(reader)?;
    match header.as_bytes().first() {
        Some(b'+') => Ok(()),
        Some(b'-') => Err(format!("redis error: {}", &header[1..])),
        _ => Err(format!("unexpected reply: {header:?}")),
    }
}

/// Read one CRLF-terminated reply line, without the trailing CRLF.
fn read_reply_line(reader: &mut impl BufRead) -> Result<String, String> {
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?
        == 0
    {
        return Err("redis closed the connection".to_string());
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Find `name` in an `a=b&c=d` query string and percent-decode its value.
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| percent_decode(v))
    })
}

/// Percent-decode a URL component. `%XX` becomes its byte; `+` is left as-is
/// (we never encode spaces as `+`). Invalid escapes are passed through verbatim.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // `%XX` (two hex digits) decodes to one byte; anything else passes through.
        if bytes[i] == b'%' {
            if let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|b| hex_val(*b)),
                bytes.get(i + 2).and_then(|b| hex_val(*b)),
            ) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Value of a single hex digit, or `None` if it isn't one.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod single_flight_tests {
    use super::*;

    #[test]
    fn external_run_identity_is_canonical_lowercase() {
        assert_eq!(
            parse_arg_tree(&format!("req={}", "a".repeat(40)))
                .ok()
                .as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        let error = parse_arg_tree(&format!("req={}", "A".repeat(40))).unwrap_err();
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn top_level_cache_hit_waits_for_an_existing_flights_outcome() {
        let request = "0".repeat(40);
        let ancestor = "b".repeat(40);
        let owner = match join_flight(&request, std::slice::from_ref(&ancestor)) {
            Flight::Owner(owner) => owner,
            _ => panic!("first arrival was not the owner"),
        };

        // This caller read an older cached value while the existing executor
        // got a transient cache error and is producing a newer outcome. It must
        // join that executor instead of publishing the stale cache hit itself.
        let stale_cache = format!("tree {}", "c".repeat(40));
        let request_for_hit = request.clone();
        let cache_hit = std::thread::spawn(move || {
            complete_cache_hit(&request_for_hit, &[], stale_cache, |_| {
                panic!("cache-hit waiter attempted to publish")
            })
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            let joined = flights()
                .lock()
                .expect("flights lock")
                .get(&request)
                .is_some_and(|flight| flight.waiters.len() == 1);
            if joined {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cache-hit caller did not join the existing flight"
            );
            std::thread::yield_now();
        }
        assert!(!cache_hit.is_finished());

        let newer = format!("tree {}", "d".repeat(40));
        assert_eq!(
            owner.finish_with(Ok(newer.clone()), |_| Ok(())),
            Ok(newer.clone())
        );
        assert_eq!(cache_hit.join().unwrap(), Ok(newer));
    }

    #[test]
    fn waiter_receives_the_owners_outcome() {
        let request = "d".repeat(40);
        let owner = match join_flight(&request, &[]) {
            Flight::Owner(owner) => owner,
            _ => panic!("first arrival was not the owner"),
        };
        let (rx, guard) = match join_flight(&request, &[]) {
            Flight::Waiter(rx, guard) => (rx, guard),
            _ => panic!("second arrival did not wait"),
        };
        let outcome = Ok(format!("blob {}", "a".repeat(40)));
        owner.finish(&outcome);
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), outcome);
        drop(guard);
    }

    #[test]
    fn top_level_publication_happens_before_the_flight_is_released() {
        let request = "7".repeat(40);
        let ancestor = "4".repeat(40);
        // Promise resolution may be the executor even though an external run
        // joins later and is the participant that requires durable publication.
        let owner = match join_flight(&request, std::slice::from_ref(&ancestor)) {
            Flight::Owner(owner) => owner,
            _ => panic!("first arrival was not the owner"),
        };
        let (rx, guard) = match join_flight(&request, &[]) {
            Flight::Waiter(rx, guard) => (rx, guard),
            _ => panic!("second arrival did not wait"),
        };

        // A caught worker failure is itself a valid tree result, but is not
        // cached. A retry may therefore produce a later success. While the
        // earlier result is being published, that retry must still be a waiter
        // rather than a new owner that could publish first.
        let caught_failure = format!("tree {}", "6".repeat(40));
        let mut published = None;
        let outcome = owner.finish_with(Ok(caught_failure.clone()), |result| {
            let (late_rx, late_guard) = match join_flight(&request, &[]) {
                Flight::Waiter(rx, guard) => (rx, guard),
                _ => panic!("flight was released before result publication"),
            };
            drop(late_rx);
            drop(late_guard);
            published = Some(result.to_string());
            Ok(())
        });
        assert_eq!(outcome, Ok(caught_failure.clone()));
        assert_eq!(published.as_deref(), Some(caught_failure.as_str()));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Ok(caught_failure)
        );
        drop(guard);

        // Only after publication completes may the retry become owner and
        // replace that result with its successful outcome.
        let successor = match join_flight(&request, &[]) {
            Flight::Owner(owner) => owner,
            _ => panic!("published flight did not admit a successor"),
        };
        let success = format!("tree {}", "5".repeat(40));
        let outcome = successor.finish_with(Ok(success.clone()), |result| {
            published = Some(result.to_string());
            Ok(())
        });
        assert_eq!(outcome, Ok(success.clone()));
        assert_eq!(published.as_deref(), Some(success.as_str()));
    }

    #[test]
    fn publication_failure_is_broadcast_before_a_retry_can_own() {
        let request = "3".repeat(40);
        let ancestor = "2".repeat(40);
        let owner = match join_flight(&request, std::slice::from_ref(&ancestor)) {
            Flight::Owner(owner) => owner,
            _ => panic!("first arrival was not the owner"),
        };
        let (rx, guard) = match join_flight(&request, &[]) {
            Flight::Waiter(rx, guard) => (rx, guard),
            _ => panic!("external arrival did not wait"),
        };

        let outcome = owner.finish_with(Ok(format!("tree {}", "1".repeat(40))), |_| {
            Err(HttpError::new(503, "pin failed"))
        });
        assert_eq!(outcome, Err((503, "pin failed".to_string())));
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), outcome);
        drop(guard);

        let replacement = match join_flight(&request, std::slice::from_ref(&ancestor)) {
            Flight::Owner(owner) => owner,
            _ => panic!("publication failure did not release the flight"),
        };
        drop(replacement);
    }

    #[test]
    fn a_stale_cache_miss_rereads_after_becoming_owner() {
        let request = "9".repeat(40);
        let first_owner = match join_flight(&request, &[]) {
            Flight::Owner(owner) => owner,
            _ => panic!("first arrival was not the owner"),
        };

        // This arrival observes a miss while the first owner is live, then is
        // descheduled until that owner has cached and left the flight table.
        let mut cached = None;
        assert!(cached.is_none());
        let cached_result = format!("blob {}", "8".repeat(40));
        cached = Some(cached_result.clone());
        first_owner.finish(&Ok(cached_result.clone()));

        let mut reread_saw_new_owner = false;
        let disposition = claim_flight_after_miss(&request, &[], || {
            // The re-read happens only after this stale arrival has claimed a
            // new flight, so another arrival must observe it as a waiter.
            match join_flight(&request, &[]) {
                Flight::Waiter(rx, guard) => {
                    reread_saw_new_owner = true;
                    drop(rx);
                    drop(guard);
                }
                _ => panic!("cache re-read ran before flight ownership"),
            }
            Ok(cached.clone())
        });

        assert!(reread_saw_new_owner);
        match disposition {
            FlightDisposition::Complete {
                outcome,
                cache_hit,
                owner,
            } => {
                assert!(cache_hit);
                assert_eq!(outcome, Ok(cached_result));
                owner
                    .expect("post-claim cache hit must retain its owner")
                    .finish(&outcome);
            }
            FlightDisposition::Run(owner) => {
                drop(owner);
                panic!("stale cache miss dispatched duplicate work");
            }
        }

        let replacement = match join_flight(&request, &[]) {
            Flight::Owner(owner) => owner,
            _ => panic!("cache-filled flight was not released"),
        };
        drop(replacement);
    }

    #[test]
    fn losing_an_owner_wakes_waiters_and_allows_a_new_owner() {
        let request = "e".repeat(40);
        let owner = match join_flight(&request, &[]) {
            Flight::Owner(owner) => owner,
            _ => panic!("first arrival was not the owner"),
        };
        let (rx, guard) = match join_flight(&request, &[]) {
            Flight::Waiter(rx, guard) => (rx, guard),
            _ => panic!("second arrival did not wait"),
        };
        drop(owner);
        let error = rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.0, 500);
        assert!(error.1.contains("owner"), "{}", error.1);
        drop(guard);

        let replacement = match join_flight(&request, &[]) {
            Flight::Owner(owner) => owner,
            _ => panic!("proven owner loss did not release the flight"),
        };
        drop(replacement);
    }

    #[test]
    fn completed_flight_removes_wait_edges_before_waiters_wake() {
        let request = "f".repeat(40);
        let ancestor = "1".repeat(40);
        let owner = match join_flight(&request, &[]) {
            Flight::Owner(owner) => owner,
            _ => panic!("first arrival was not the owner"),
        };
        let (rx, guard) = match join_flight(&request, std::slice::from_ref(&ancestor)) {
            Flight::Waiter(rx, guard) => (rx, guard),
            _ => panic!("second arrival did not wait"),
        };
        assert!(park_would_deadlock(
            &ancestor,
            std::slice::from_ref(&request)
        ));

        let outcome = Ok(format!("blob {}", "b".repeat(40)));
        owner.finish(&outcome);

        // Deliberately leave the result unread and the guard alive: completion
        // itself, rather than waiter scheduling, owns removal of the edge.
        assert!(!park_would_deadlock(
            &ancestor,
            std::slice::from_ref(&request)
        ));
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), outcome);
        drop(guard);
    }

    #[test]
    fn old_waiter_guard_cannot_remove_a_new_flights_edge() {
        let request = "a".repeat(40);
        let ancestor = "2".repeat(40);
        let first_owner = match join_flight(&request, &[]) {
            Flight::Owner(owner) => owner,
            _ => panic!("first arrival was not the owner"),
        };
        let (first_rx, first_guard) = match join_flight(&request, std::slice::from_ref(&ancestor)) {
            Flight::Waiter(rx, guard) => (rx, guard),
            _ => panic!("second arrival did not wait"),
        };
        let first_outcome = Ok(format!("blob {}", "c".repeat(40)));
        first_owner.finish(&first_outcome);

        let second_owner = match join_flight(&request, &[]) {
            Flight::Owner(owner) => owner,
            _ => panic!("completed flight did not admit a new owner"),
        };
        let (second_rx, second_guard) = match join_flight(&request, std::slice::from_ref(&ancestor))
        {
            Flight::Waiter(rx, guard) => (rx, guard),
            _ => panic!("new flight's second arrival did not wait"),
        };
        assert!(park_would_deadlock(
            &ancestor,
            std::slice::from_ref(&request)
        ));

        // The first guard is deliberately late. Its edge was already cleared
        // by first-flight completion; dropping it must not remove the identical
        // edge registered by the second flight.
        drop(first_guard);
        assert!(park_would_deadlock(
            &ancestor,
            std::slice::from_ref(&request)
        ));

        let second_outcome = Ok(format!("blob {}", "d".repeat(40)));
        second_owner.finish(&second_outcome);
        assert_eq!(
            first_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            first_outcome
        );
        assert_eq!(
            second_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            second_outcome
        );
        drop(second_guard);
    }
}

#[cfg(test)]
mod continuation_shape_tests {
    use super::*;
    use std::io::Write;
    use std::process::Stdio;

    fn shape(
        has_input: bool,
        has_map: bool,
        has_run: bool,
        has_request: bool,
        has_then: bool,
        catch: bool,
    ) -> Result<(), HttpError> {
        validate_continuation_shape(
            "test-continuation",
            has_input,
            has_map,
            has_run,
            has_request,
            false,
            has_then,
            catch,
        )
    }

    /// The `eval` middle step obeys the same shape rules as `run`: exclusive
    /// with the other middle forms, needs `in`, and a `catch` needs a `then`.
    fn eval_shape(
        has_input: bool,
        has_other_middle: bool,
        has_then: bool,
        catch: bool,
    ) -> Result<(), HttpError> {
        validate_continuation_shape(
            "test-continuation",
            has_input,
            has_other_middle,
            false,
            false,
            true,
            has_then,
            catch,
        )
    }

    #[test]
    fn eval_is_exclusive_with_the_other_middle_steps() {
        let error = eval_shape(true, true, false, false).unwrap_err();
        assert!(
            error.message().contains("mutually exclusive"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn eval_runs_alone_or_with_a_callback() {
        assert!(eval_shape(true, false, false, false).is_ok());
        assert!(eval_shape(true, false, true, false).is_ok());
    }

    #[test]
    fn eval_catch_requires_a_callback() {
        let error = eval_shape(true, false, false, true).unwrap_err();
        assert!(
            error.message().contains("without 'then'"),
            "{}",
            error.message()
        );
        assert!(eval_shape(true, false, true, true).is_ok());
    }

    #[test]
    fn exact_request_is_exclusive_with_input_map_and_run() {
        for (input, map, run) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let error = shape(input, map, run, true, false, false).unwrap_err();
            assert!(
                error.message().contains("mutually exclusive")
                    || error.message().contains("both 'request' and 'in'"),
                "{}",
                error.message()
            );
        }
    }

    #[test]
    fn exact_request_allows_a_result_callback_or_plain_tail_call() {
        assert!(shape(false, false, false, true, false, false).is_ok());
        assert!(shape(false, false, false, true, true, false).is_ok());
    }

    #[test]
    fn exact_request_catch_requires_a_callback() {
        let error = shape(false, false, false, true, false, true).unwrap_err();
        assert!(error.message().contains("without 'then'"));
        assert!(shape(false, false, false, true, true, true).is_ok());
    }

    #[test]
    fn result_pin_retries_a_transient_ref_lock() {
        let git_dir = std::env::temp_dir().join(format!(
            "caos-pin-result-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let init = Command::new("git")
            .args(["init", "--bare", git_dir.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );

        let mut hash_object = Command::new("git")
            .args([
                "-C",
                git_dir.to_str().unwrap(),
                "hash-object",
                "-w",
                "--stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        hash_object
            .stdin
            .take()
            .unwrap()
            .write_all(b"result")
            .unwrap();
        let object = hash_object.wait_with_output().unwrap();
        assert!(object.status.success());
        let hash = String::from_utf8(object.stdout).unwrap().trim().to_string();

        let refname = "refs/caos/res/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let lock = git_dir.join(format!("{refname}.lock"));
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(&lock, b"held").unwrap();
        let unlock = std::thread::spawn({
            let lock = lock.clone();
            move || {
                std::thread::sleep(Duration::from_millis(25));
                std::fs::remove_file(lock).unwrap();
            }
        });

        if let Err(error) = pin_result_in(git_dir.to_str().unwrap(), refname, &hash) {
            panic!("{}", error.message());
        }
        unlock.join().unwrap();
        let pinned = Command::new("git")
            .args(["-C", git_dir.to_str().unwrap(), "rev-parse", refname])
            .output()
            .unwrap();
        assert!(pinned.status.success());
        assert_eq!(String::from_utf8(pinned.stdout).unwrap().trim(), hash);
        std::fs::remove_dir_all(git_dir).unwrap();
    }
}
