//! Compute: the `/run` pipeline.
//!
//! A request is a content-addressed tree `{args, std, salt}` — the worker image
//! rides inside `args` under a reserved `image` entry; `/run?req=<hash>`
//! reads it, then: cache lookup (Redis) → run-cycle detection → image resolution
//! (a `docker://` ref used as-is, or a git-docker image converted and pushed to
//! the registry) → dispatch through the runner rendezvous ([`crate::runner`]:
//! the job is matched to a long-polling runner, which posts back
//! `"<type> <hash>"`) — or `"promise <hash>"`, a map-then continuation the
//! worker left behind instead of a value, which [`resolve_promise`] resolves
//! *after* the worker has moved on (see `design/map-then.md`). A top-level run
//! also pins `refs/caos/res/<req>` at the (fully resolved) result. Results,
//! converted images, and built layers are all cached in Redis (best-effort).
//!
//! Workers never wait on other workers: a worker either computes a value or
//! describes the remaining work (its promise) and finishes its job. Only server
//! threads block, so any number of concurrent runs cannot deadlock — capacity
//! lives runner-side (the set of parked polls), not in a server semaphore.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::storage::{fetch_blob, fetch_tree, store_git_blob, store_git_tree};
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

/// `GET /run?req=<reqHash>` — run the request object `<reqHash>` (a tree
/// `{args, std, salt}`, the worker image inside `args`) and return its result
/// as `"<type> <hash>"`.
///
/// The request being a content-addressed object means `reqHash` *is* the cache
/// key (it captures args — hence the image — plus std and salt) and the
/// rendezvous id: an external run also pins `refs/caos/res/<reqHash>` at the
/// result, so a client can fetch it by ref. Only external callers reach this
/// endpoint now (the CLI, which pushed the request): workers never call back
/// into `/run` — a worker's sub-runs are promise resolutions the server
/// performs itself ([`run_req`] recursion).
pub(crate) fn run(config: &Config, query: &str) -> Result<Vec<u8>, HttpError> {
    let req = query_param(query, "req")
        .ok_or_else(|| HttpError::new(400, "missing 'req' query parameter"))?;
    if req.is_empty() || !req.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HttpError::new(400, format!("invalid req hash: {req:?}")));
    }
    let trace_id = query_param(query, "trace");
    if let Some(id) = &trace_id {
        if !crate::trace::valid_id(id) {
            return Err(HttpError::new(400, "invalid trace id"));
        }
        config.trace.begin(id).map_err(|e| HttpError::new(409, e))?;
    }
    // An HTTP run is by definition top-level: the run stack (cycle detection)
    // exists only inside the server, threaded through promise sub-runs.
    let result = run_req(config, &req, &[], trace_id.as_deref());
    if let Some(id) = &trace_id {
        config.trace.end(id);
    }
    let result = result?;
    // Pin an external run's result so a client can fetch it by ref and it
    // survives gc; sub-runs set no ref (they'd flood the namespace).
    pin_result(config, &req, &result);
    Ok(format!("{result}\n").into_bytes())
}

/// Run request `req` with `stack` the chain of ancestor request hashes (empty =
/// top-level), returning the fully-resolved `"<type> <hash>"`. The whole
/// pipeline behind both `GET /run` and promise sub-runs: cache lookup →
/// run-cycle detection → the container run → promise resolution → cache store.
fn run_req(
    config: &Config,
    req: &str,
    stack: &[String],
    trace_id: Option<&str>,
) -> Result<String, HttpError> {
    let span_id = trace_id.and_then(|id| config.trace.start(id));
    let result = run_req_inner(config, req, stack, trace_id, span_id);
    if let (Some(trace_id), Some(span_id)) = (trace_id, span_id) {
        config.trace.finish(trace_id, span_id);
    }
    result
}

fn run_req_inner(
    config: &Config,
    req: &str,
    stack: &[String],
    trace_id: Option<&str>,
    span_id: Option<u64>,
) -> Result<String, HttpError> {
    // Unpack the request: args (a tree; the worker image is its reserved `image`
    // entry — an embedded tree for a git image, a ref blob for `docker://`), std
    // (a ref blob), salt (an opaque blob). `std` names the standard library,
    // materialized at `/cas/std` in the worker; `salt` is a cache-buster. Both
    // are part of the request (hence the key), threaded into the worker, and
    // inherited by any promise sub-runs this request leaves behind.
    let (image, args, std, salt) = read_request(config, req)?;
    let traced_arg_entries = if trace_id.is_some() && span_id.is_some() {
        Some(args_entries(config, &args)?)
    } else {
        None
    };
    if let (Some(trace_id), Some(span_id), Some(entries)) =
        (trace_id, span_id, traced_arg_entries.as_ref())
    {
        config.trace.inputs(trace_id, span_id, entries);
    }
    if image.is_empty() {
        return Err(HttpError::new(400, "request has empty image"));
    }
    // The args hash is interpolated into `--args=`; require a plain hex object id.
    if args.is_empty() || !args.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HttpError::new(400, format!("invalid args hash: {args:?}")));
    }
    if !std.is_empty() && !std.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HttpError::new(400, format!("invalid std hash: {std:?}")));
    }

    // The request hash is the cache key (it captures args+std+salt); the value is
    // the final result "<type> <hash>" — a promise is resolved before it's cached,
    // so a hit never re-resolves. A hit skips image conversion and the container
    // run. Redis is best-effort: a lookup error just means we run uncached.
    let key = format!("caos:result:{req}");
    match cache_get(&config.redis_addr, &key) {
        Ok(Some(result)) => {
            if let (Some(trace_id), Some(span_id)) = (trace_id, span_id) {
                config.trace.cache(trace_id, span_id, true);
            }
            eprintln!("cache hit: req={req} -> {result}");
            return Ok(result);
        }
        Ok(None) => {
            if let (Some(trace_id), Some(span_id)) = (trace_id, span_id) {
                config.trace.cache(trace_id, span_id, false);
            }
            eprintln!("cache miss: req={req} (image={image} args={args}); running worker")
        }
        Err(e) => eprintln!("cache lookup failed ({e}); running worker: req={req}"),
    }

    // Re-entering a request already on the stack has no fixpoint — fail, listing
    // the cycle. (A cache hit can't be on the stack: a cyclic computation never
    // completes, so it never caches, which is why checking only on a miss is
    // sound.) The request hash is exactly this frame's identity.
    if let Some(pos) = stack.iter().position(|f| f == req) {
        let mut cycle: Vec<&str> = stack[pos..].iter().map(String::as_str).collect();
        cycle.push(req);
        let listing = cycle.join("\n  -> ");
        eprintln!("run cycle detected:\n  {listing}");
        return Err(HttpError::new(
            400,
            format!("run cycle detected:\n  {listing}"),
        ));
    }
    // Promise sub-runs see this computation as an ancestor.
    let mut child_stack: Vec<String> = stack.to_vec();
    child_stack.push(req.to_string());

    // Run the worker through the runner rendezvous: resolve the image to a
    // docker-pullable ref (always sent — a warm runner that pinned the image
    // ignores it, and conversion is Redis-cached, so re-resolving is a lookup),
    // read the args tree's top level (what runners' required args match
    // against), and hand the job to a polling runner. The dispatch blocks this
    // server thread until a runner posts the result; capacity is runner-side
    // (the set of parked polls), so there's no server-side slot to hold.
    let result = {
        let image_ref = resolve_image(config, &image)?;
        let arg_entries = match traced_arg_entries {
            Some(entries) => entries,
            None => args_entries(config, &args)?,
        };
        crate::runner::dispatch(req, arg_entries, &image_ref)?
    };

    if result_hash(&result).is_empty() {
        eprintln!("worker produced no result on stdout: req={req}");
        return Err(HttpError::new(500, "worker produced no result on stdout"));
    }

    // A promise is not a value: the worker exited leaving a map-then continuation
    // behind. Resolve it — the container (and its slot) are already gone.
    let result = match result.split_once(' ') {
        Some((PROMISE_KIND, cont)) => {
            eprintln!("resolving promise: req={req} -> continuation {cont}");
            resolve_promise(config, cont, &std, &salt, &child_stack, trace_id)?
        }
        _ => result,
    };

    // Cache the (resolved) result for next time (best-effort).
    match cache_set(&config.redis_addr, &key, &result) {
        Ok(()) => eprintln!("ran worker: req={req} -> {result} (cached)"),
        Err(e) => eprintln!("ran worker: req={req} -> {result} (cache store failed: {e})"),
    }

    Ok(result)
}

// ---- Promise resolution ------------------------------------------------------

/// Resolve a continuation — a tree `{in, map?, run?, then?}` where `in` is a
/// real tree entry (the data node) and `map`/`run`/`then` are blobs naming
/// images (see `design/map-then.md`). `map` and `run` are mutually exclusive
/// (the client already refuses to record both; this is defense in depth). One
/// resolution path covers both forms — a *middle step*, then `then`:
///
/// 1. if `map` is given: run `map --in=<child>` for each child of `in` in
///    parallel (a blob `in` is a leaf — no children), assembling the results
///    into a `children` tree under the original names;
/// 2. if `run` is given: one sub-run, `run(--in=<in>)` — the single-valued
///    form. Its result R may be any kind (a commit as much as a blob/tree);
/// 3. the result is `then(--in=<in>[, --children=<children> | --result=<R>])`
///    if `then` is given (the extra arg only when a middle step ran), else the
///    middle step's own result — the `children` tree, or R. With no middle
///    step, `then(--in=<in>)` is a plain tail call.
///
/// Every sub-run goes through [`run_req`], so promises nest arbitrarily (a map
/// child, a `run`, or a `then` may itself promise) and each sub-run gets its
/// own memoization and cycle detection (via `stack`).
fn resolve_promise(
    config: &Config,
    cont: &str,
    std: &str,
    salt: &str,
    stack: &[String],
    trace_id: Option<&str>,
) -> Result<String, HttpError> {
    use gix::objs::tree::EntryKind;

    let mut input: Option<gix::objs::tree::Entry> = None;
    let (mut map, mut run, mut then) = (None, None, None);
    for entry in fetch_tree(config, cont)
        .map_err(|e| HttpError::new(500, format!("reading continuation {cont}: {e}")))?
    {
        match entry.name.as_str() {
            "in" => input = Some(named_entry("in", entry.mode, entry.oid)),
            "map" => map = Some(blob_string(config, &entry.oid.to_string())?),
            "run" => run = Some(blob_string(config, &entry.oid.to_string())?),
            "then" => then = Some(blob_string(config, &entry.oid.to_string())?),
            other => {
                return Err(HttpError::new(
                    500,
                    format!("continuation {cont} has unknown entry {other:?}"),
                ))
            }
        }
    }
    let input =
        input.ok_or_else(|| HttpError::new(500, format!("continuation {cont} missing 'in'")))?;
    if map.is_some() && run.is_some() {
        return Err(HttpError::new(
            500,
            format!("continuation {cont} has both 'map' and 'run' (they are mutually exclusive)"),
        ));
    }
    if map.is_none() && run.is_none() && then.is_none() {
        return Err(HttpError::new(
            500,
            format!("continuation {cont} has none of 'map', 'run', or 'then'"),
        ));
    }

    // The middle step, if any: `map` fans out over `in`'s children and yields a
    // `children` tree; `run` is one sub-run yielding a `result` entry. Either
    // way we get (the extra arg `then` receives, the result when there is no
    // `then`).
    let mid: Option<(gix::objs::tree::Entry, String)> = if let Some(img) = &map {
        // Map the children in parallel — one thread per child, each a full
        // [`run_req`] (so a child may itself promise). Concurrency is bounded by
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
                                let result =
                                    run_image(config, img, vec![arg], std, salt, stack, trace_id)?;
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
        ))
    } else if let Some(img) = &run {
        // The single-valued form: `run(--in=<in>)`, fully resolved by [`run_req`]
        // (so a promise R leaves behind is already collapsed to a value here).
        let result = run_image(config, img, vec![input.clone()], std, salt, stack, trace_id)?;
        Some((result_entry("result", &result)?, result))
    } else {
        None
    };

    match (then, mid) {
        // `then` combines: it gets the original `in`, plus the middle step's
        // contribution when one ran — (`--in`, `--children`) after a map,
        // (`--in`, `--result`) after a run, bare `--in` for a plain tail call.
        (Some(img), mid) => {
            let mut args = vec![input];
            if let Some((extra, _)) = mid {
                args.push(extra);
            }
            run_image(config, &img, args, std, salt, stack, trace_id)
        }
        // No `then`: the middle step's own result is the request's result.
        (None, Some((_, result))) => Ok(result),
        // Unreachable — the presence check above requires some step.
        (None, None) => Err(HttpError::new(
            500,
            format!("continuation {cont} has no step to run"),
        )),
    }
}

/// Run image `image_ref` over the given call args as a promise sub-run: unwrap
/// any curry layers, build the args tree (worker image folded in under its
/// reserved `image` entry) and the request object `{args, std, salt}`
/// server-side — byte-identical to what a client would build, so the request
/// hash (and cache key) is the same no matter who assembles it — and send it
/// through [`run_req`]. Returns `"<type> <hash>"`.
fn run_image(
    config: &Config,
    image_ref: &str,
    call_args: Vec<gix::objs::tree::Entry>,
    std: &str,
    salt: &str,
    stack: &[String],
    trace_id: Option<&str>,
) -> Result<String, HttpError> {
    use gix::objs::tree::EntryKind;

    let (image, bound) = unwrap_curry(config, image_ref)?;
    let store_err = |e: String| HttpError::new(500, format!("building sub-request: {e}"));

    // The worker image rides *in* the args tree under the reserved `image` entry
    // (embedded as the image's own tree for a git image, a ref blob for
    // `docker://`) — the same shape the client builds, merged last so the
    // reserved name wins over any like-named user arg.
    let image_entry = if image.len() == 40 && image.bytes().all(|b| b.is_ascii_hexdigit()) {
        let oid = gix::ObjectId::from_hex(image.as_bytes())
            .map_err(|e| HttpError::new(500, format!("invalid image hash: {e}")))?;
        named_entry("image", EntryKind::Tree.into(), oid)
    } else {
        named_entry(
            "image",
            EntryKind::Blob.into(),
            store_git_blob(config, image.as_bytes()).map_err(store_err)?,
        )
    };
    let args = merge_entries(merge_entries(bound, call_args), vec![image_entry]);
    let args_tree = store_git_tree(config, args).map_err(store_err)?;

    let entries = vec![
        named_entry("args", EntryKind::Tree.into(), args_tree),
        named_entry(
            "std",
            EntryKind::Blob.into(),
            store_git_blob(config, std.as_bytes()).map_err(store_err)?,
        ),
        named_entry(
            "salt",
            EntryKind::Blob.into(),
            store_git_blob(config, salt.as_bytes()).map_err(store_err)?,
        ),
    ];
    let req = store_git_tree(config, entries).map_err(store_err)?;
    run_req(config, &req.to_string(), stack, trace_id)
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

/// The args tree's top-level name → oid map — what a runner's required args are
/// matched against (pure oid equality; see `crate::runner`).
fn args_entries(
    config: &Config,
    args: &str,
) -> Result<std::collections::BTreeMap<String, String>, HttpError> {
    let entries = fetch_tree(config, args)
        .map_err(|e| HttpError::new(400, format!("reading args tree: {e}")))?;
    Ok(entries
        .into_iter()
        .map(|e| (e.name, e.oid.to_string()))
        .collect())
}

/// Unpack a request object (a tree `{args, std, salt}`) into its parts: the image
/// ref (read from the args tree's reserved `image` entry), the args-tree hash, the
/// std-tree hash (empty if none), and the salt (empty if none).
fn read_request(config: &Config, req: &str) -> Result<(String, String, String, String), HttpError> {
    let entries = fetch_tree(config, req)
        .map_err(|e| HttpError::new(400, format!("reading request: {e}")))?;
    let mut args = None;
    let mut std = String::new();
    let mut salt = String::new();
    for entry in entries {
        match entry.name.as_str() {
            "args" => args = Some(entry.oid.to_string()),
            "std" => std = blob_string(config, &entry.oid.to_string())?,
            "salt" => salt = blob_string(config, &entry.oid.to_string())?,
            _ => {}
        }
    }
    let args = args.ok_or_else(|| HttpError::new(400, "request missing 'args'"))?;
    // The worker image rides in the args tree under the reserved `image` entry.
    let image = read_args_image(config, &args)?;
    Ok((image, args, std, salt))
}

/// Read the worker image ref out of an args tree — its reserved `image` entry. A
/// git-docker image *is* a git tree, so it rides embedded: the entry is a tree and
/// its oid is the image hash (the image thus travels inside the request graph). A
/// `docker://` image has no git object, so it rides as a blob naming the registry
/// ref.
fn read_args_image(config: &Config, args: &str) -> Result<String, HttpError> {
    let entries = fetch_tree(config, args)
        .map_err(|e| HttpError::new(400, format!("reading args tree: {e}")))?;
    for entry in entries {
        if entry.name == "image" {
            return if entry.mode.is_tree() {
                Ok(entry.oid.to_string())
            } else {
                blob_string(config, &entry.oid.to_string())
            };
        }
    }
    Err(HttpError::new(400, "request args missing 'image'"))
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

/// Pin `refs/caos/res/<req>` at the result so a client can fetch it by ref and it
/// survives gc. Best-effort: a failure just means the result isn't ref-pinned
/// (it's still cached and reachable by hash). `result` is `"<type> <hash>"`.
fn pin_result(config: &Config, req: &str, result: &str) {
    let hash = result_hash(result);
    if hash.is_empty() {
        return;
    }
    let refname = format!("refs/caos/res/{req}");
    match Command::new("git")
        .args(["-C", &config.git_dir, "update-ref", &refname, hash])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("warning: git update-ref {refname} exited with {status}"),
        Err(e) => eprintln!("warning: pinning {refname}: {e}"),
    }
}

/// Resolve the `image` parameter to a reference the host docker daemon can run.
///
/// `docker://<ref>` is an ordinary docker reference, used as-is. Anything else is
/// one of our git images (the default): convert it to a real image, push it to
/// the registry, and return a digest reference into the registry.
fn resolve_image(config: &Config, image: &str) -> Result<String, HttpError> {
    if let Some(reference) = image.strip_prefix(DOCKER_SCHEME) {
        if reference.is_empty() || reference.starts_with('-') {
            return Err(HttpError::new(
                400,
                format!("invalid docker image: {reference:?}"),
            ));
        }
        return Ok(reference.to_string());
    }
    if !image.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HttpError::new(
            400,
            format!("git image must be a hex hash (or use {DOCKER_SCHEME}<ref>): {image:?}"),
        ));
    }
    convert_git_image(config, image)
        .map_err(|e| HttpError::new(500, format!("converting git image {image}: {e}")))
}

/// Convert the git-docker image tree `git_hash` to a real image and push it to
/// the registry, returning a digest reference. Cached in Redis by git hash.
fn convert_git_image(config: &Config, git_hash: &str) -> Result<String, String> {
    let image_key = format!("caos:image:{git_hash}");
    if let Ok(Some(manifest_digest)) = cache_get(&config.redis_addr, &image_key) {
        eprintln!("image cache hit: {git_hash} -> {manifest_digest}");
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
        if base_ref.is_empty() {
            return Err("base blob is empty".to_string());
        }
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
    let base = config.registry_push_url.trim_end_matches('/');
    let url = format!("{base}/v2/{REGISTRY_REPO}/manifests/{digest}");
    let response = minreq::put(&url)
        .with_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
        .with_body(data.to_vec())
        .send()
        .map_err(|e| format!("PUT {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "uploading manifest {digest}: {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    Ok(())
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
