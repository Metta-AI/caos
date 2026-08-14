//! caos core-seeder-runner: the bootstrap answerer (design/caos-expr.md,
//! Phase 3, "core-seeder-runner").
//!
//! The irreducible core (`flake-builder`, and later `runner`/`cargo`/`rustc`/
//! `deep-deps`) cannot be built by the machinery it *is*: forming
//! `flake-builder`'s image by running `flake-builder` is a cycle. So
//! `bootstrap`/`build-builtins.sh` hand-builds each core artifact and publishes
//! a **seed record** per item under a ref (default `refs/caos/seed`):
//!
//! ```text
//! refs/caos/seed -> tree {
//!   <name> -> tree {
//!     required -> blob (JSON: { "<argname>": "<oid>", ... })
//!     result   -> the pre-built object (a tree for an image, a blob otherwise)
//!   }
//! }
//! ```
//!
//! This runner reads those records and long-polls the server's
//! `POST /runner/poll` (the ordinary runner protocol — *no server change*), one
//! poll per record with `required` = that record's arg-tree entries. On a match
//! it **posts the pre-built result directly** (`POST /runner/result`), spawning
//! no container. It wins over `runnerd` automatically: the server prefers the
//! most-specific `required` match (`offer_job` / `best_pending`
//! `max_by_key(required.len())`), and a full arg-tree match beats `runnerd`'s
//! empty `required` — even when no generic runner exists (the bootstrap
//! situation).
//!
//! ## No live answerer (yet)
//!
//! The design sketches a *live answerer* — a thread answering from the current
//! seed map while a main thread extends it in dependency order — because
//! forming a core item's key can dispatch *another* core item's build (e.g.
//! cargo's expr runs std/flake-builder). That is only needed once we convert
//! a core item whose `.caos-expr` names another *core std* item as its
//! builder/image. `flake-builder` names the `docker://seeded` sentinel, which
//! `resolve_expr_image` passes straight through with **no dispatch**, so
//! forming its key runs nothing and this runner just answers a static set of
//! records. The live answerer arrives with the first item that runs another
//! (cargo → flake-builder).
//!
//! Configuration (env):
//!   `CAOS_SERVER_URL`  the server to poll (default `http://127.0.0.1`)
//!   `CAOS_GIT_DIR`     the server's git repo, read for the seed records
//!                      (required — the records live there, colocated)
//!   `CAOS_SEED_REF`    the seed ref (default `refs/caos/seed`)
//!   `CAOS_RUNNER_TOKEN` bearer token, if the server requires one

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The seed ref, unless `CAOS_SEED_REF` overrides it.
const DEFAULT_SEED_REF: &str = "refs/caos/seed";

/// How long each poll hangs before re-polling (a reconnect cadence; a seeder
/// never idles out).
///
/// This is also HOW STALE A PARKED POLL CAN BE. A poll carries the `required`
/// its record had when it parked, so a republish that changes a record's key
/// keeps being advertised under the OLD key until the poll turns over. The
/// server treats a parked-but-disagreeing seeder as a permanent mismatch once
/// `CAOS_SEEDED_GRACE_SECS` has passed (server/runner.rs `seeded_verdict`), so
/// this bound plus `RESCAN` is what that grace has to clear. Kept short — five
/// records re-polling every 20s is 15 requests a minute, and it buys both a
/// tighter grace and a faster pickup of a republished record.
const POLL_TTL: Duration = Duration::from_secs(20);

/// Backoff after a failed poll or a git read (server or repo not ready).
const RETRY: Duration = Duration::from_secs(2);

/// How often the main loop re-reads the seed ref to pick up newly published
/// records (the host publishes std+seed AFTER the stack is up).
const RESCAN: Duration = Duration::from_secs(5);

struct Config {
    server_url: String,
    git_dir: String,
    seed_ref: String,
    token: Option<String>,
}

/// One seed record: the arg-tree entries a matching job must carry, and the
/// pre-built `"<kind> <hash>"` to answer with.
#[derive(Clone)]
struct Record {
    name: String,
    required: serde_json::Map<String, serde_json::Value>,
    result: String,
}

/// The records currently on disk, by NAME — the whole of this runner's state.
///
/// Replaced wholesale on every rescan, never added to. The ref IS the complete
/// list, so anything not in it has been withdrawn, and a name that is still
/// there means "answer with THIS result now" regardless of what it said before.
type Records = Arc<Mutex<HashMap<String, Record>>>;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn main() {
    let git_dir = match std::env::var("CAOS_GIT_DIR") {
        Ok(d) if !d.is_empty() => d,
        _ => {
            eprintln!("core-seeder-runner: CAOS_GIT_DIR is required (the server's git repo)");
            std::process::exit(2);
        }
    };
    let config = Config {
        server_url: env_or("CAOS_SERVER_URL", "http://127.0.0.1"),
        git_dir,
        seed_ref: env_or("CAOS_SEED_REF", DEFAULT_SEED_REF),
        token: std::env::var("CAOS_RUNNER_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
    };
    eprintln!(
        "core-seeder-runner: server {}, seed {} in {}",
        config.server_url, config.seed_ref, config.git_dir
    );

    // ONE THREAD PER NAME, and the disk REPLACES its state on every rescan.
    //
    // Threads used to be keyed on a record's whole CONTENT — name, required and
    // result — on the reasoning that a stale one "keeps polling harmlessly (its
    // now-absent required matches no current job)". That is false whenever a
    // record is republished with the same `required` and a different `result`,
    // which is the ordinary case: `caosd up` re-runs build-builtins, and a
    // rebuilt host binary changes the flake-builder delta while its arg-tree
    // key stays exactly the same. Both threads then match every job and race,
    // so the answer alternates between the current image and a stale one.
    //
    // Observed after several `caosd up`s: TWO live answerers for flake-builder
    // and runner, THREE for cargo and rustc, handing out different results by
    // coin flip. Downstream that alternated the builder image, so the test
    // stack image changed run to run, and the suite started a fresh stack and
    // recompiled every std tool about half the time (35s against 67-85s).
    //
    // So: the ref is the complete list, a rescan replaces the map, and a
    // thread reads the CURRENT record for its name every time round — carrying
    // no snapshot of its own to go stale. A name that disappears leaves its
    // thread idling rather than answering.
    let records: Records = Arc::new(Mutex::new(HashMap::new()));
    let mut threads: HashSet<String> = HashSet::new();
    loop {
        match read_records(&config) {
            Ok(list) => {
                let names: Vec<String> = list.iter().map(|r| r.name.clone()).collect();
                {
                    let mut current = records.lock().expect("seed records");
                    for rec in list {
                        match current.get(&rec.name) {
                            Some(prev) if prev.result == rec.result => {}
                            Some(prev) => eprintln!(
                                "core-seeder-runner: {} now answers {} (was {})",
                                rec.name, rec.result, prev.result
                            ),
                            None => eprintln!(
                                "core-seeder-runner: answering {} -> {}",
                                rec.name, rec.result
                            ),
                        }
                        current.insert(rec.name.clone(), rec);
                    }
                    // Whatever the ref no longer names is withdrawn.
                    current.retain(|name, _| names.iter().any(|n| n == name));
                }
                for name in names {
                    if threads.insert(name.clone()) {
                        let server = config.server_url.clone();
                        let token = config.token.clone();
                        let records = Arc::clone(&records);
                        std::thread::spawn(move || {
                            poll_loop(&server, token.as_deref(), &name, &records)
                        });
                    }
                }
            }
            Err(e) => eprintln!("core-seeder-runner: reading {}: {e}", config.seed_ref),
        }
        std::thread::sleep(RESCAN);
    }
}

/// One NAME's poll thread: claim a matching job, post whatever that name
/// currently answers with, repeat. Never exits — a seeder is standing capacity,
/// and a name that is withdrawn may come back on the next publish.
///
/// The record is re-read from the shared map on every iteration AND again after
/// a job is claimed, because a publish can land while this poll is parked. That
/// is the whole point: the thread holds no result of its own to serve stale.
fn poll_loop(server: &str, token: Option<&str>, name: &str, records: &Records) {
    loop {
        let rec = records.lock().expect("seed records").get(name).cloned();
        let Some(rec) = rec else {
            // Withdrawn: idle rather than answer for something the ref no
            // longer lists. It costs one sleep to come back if it returns.
            std::thread::sleep(RESCAN);
            continue;
        };
        match poll(server, token, &rec) {
            Ok(Some((req, nonce))) => {
                let current = records
                    .lock()
                    .expect("seed records")
                    .get(name)
                    .map(|r| r.result.clone());
                match current {
                    Some(result) => {
                        if let Err(e) = post_result(server, token, &req, &nonce, &result) {
                            eprintln!("core-seeder-runner: {name}: posting result: {e}");
                        }
                    }
                    // Withdrawn while we were parked. Answering with the result
                    // we polled on would serve exactly the staleness this
                    // rewrite removes, so leave the job for another answerer.
                    None => eprintln!(
                        "core-seeder-runner: {name}: withdrawn while parked; not answering {req}"
                    ),
                }
            }
            Ok(None) => {} // idle or evicted: re-poll
            Err(e) => {
                eprintln!("core-seeder-runner: {name}: poll failed: {e}");
                std::thread::sleep(RETRY);
            }
        }
    }
}

/// One long-poll for `rec`. `Some((req, nonce))` to answer; `None` on idle/exit.
fn poll(
    server: &str,
    token: Option<&str>,
    rec: &Record,
) -> Result<Option<(String, String)>, String> {
    let body = serde_json::json!({
        "required": serde_json::Value::Object(rec.required.clone()),
        "lineage": [],
        "ttl_ms": POLL_TTL.as_millis() as u64,
    });
    let url = format!("{}/runner/poll", server.trim_end_matches('/'));
    let resp = post(&url, &body.to_string(), token, POLL_TTL.as_secs() + 15)?;
    if resp.status_code != 200 {
        return Err(format!(
            "poll ({}): {}",
            resp.status_code,
            resp.as_str().unwrap_or("")
        ));
    }
    let v: serde_json::Value =
        serde_json::from_str(resp.as_str().unwrap_or("")).map_err(|e| format!("bad reply: {e}"))?;
    let Some(job) = v.get("job").filter(|j| j.is_object()) else {
        return Ok(None);
    };
    let req = job["req"].as_str().unwrap_or_default().to_string();
    let nonce = job["nonce"].as_str().unwrap_or_default().to_string();
    if req.is_empty() || nonce.is_empty() {
        return Err(format!("job missing req/nonce: {job}"));
    }
    Ok(Some((req, nonce)))
}

/// Post the pre-built result for a claimed job. 200/410 both mean settled.
fn post_result(
    server: &str,
    token: Option<&str>,
    req: &str,
    nonce: &str,
    result: &str,
) -> Result<(), String> {
    let body = serde_json::json!({
        "req": req, "nonce": nonce, "ok": true, "result": result,
    });
    let url = format!("{}/runner/result", server.trim_end_matches('/'));
    let resp = post(&url, &body.to_string(), token, 30)?;
    match resp.status_code {
        200 | 410 => Ok(()),
        code => Err(format!("result ({code}): {}", resp.as_str().unwrap_or(""))),
    }
}

/// A JSON POST carrying the world header and (optionally) the bearer token.
fn post(
    url: &str,
    body: &str,
    token: Option<&str>,
    timeout_secs: u64,
) -> Result<minreq::Response, String> {
    let mut req = minreq::post(url)
        .with_header("content-type", "application/json")
        .with_header(caos_world::WORLD_HEADER, caos_world::WORLD)
        .with_timeout(timeout_secs)
        .with_body(body.to_string());
    if let Some(token) = token {
        req = req.with_header("Authorization", format!("Bearer {token}"));
    }
    req.send().map_err(|e| format!("POST {url}: {e}"))
}

/// Read the seed records from `refs/caos/seed` in the server's git repo. An
/// absent ref (not published yet) is an empty set, not an error — the host
/// publishes std+seed after the stack is up, and the main loop rescans.
fn read_records(config: &Config) -> Result<Vec<Record>, String> {
    let repo = gix::open(&config.git_dir).map_err(|e| format!("open {}: {e}", config.git_dir))?;
    let reference = match repo.find_reference(config.seed_ref.as_str()) {
        Ok(r) => r,
        // No such ref yet (or the repo has no refs): nothing to seed.
        Err(_) => return Ok(Vec::new()),
    };
    let mut reference = reference;
    let root = reference
        .peel_to_id()
        .map_err(|e| format!("peeling {}: {e}", config.seed_ref))?
        .detach();
    let mut records = Vec::new();
    for entry in read_tree(&repo, &root)? {
        if !entry.mode.is_tree() {
            continue; // records are subtrees; ignore anything else
        }
        let name = String::from_utf8_lossy(&entry.filename).into_owned();
        match read_record(&repo, &name, &entry.oid) {
            Ok(rec) => records.push(rec),
            Err(e) => eprintln!("core-seeder-runner: skipping seed record {name:?}: {e}"),
        }
    }
    Ok(records)
}

/// Parse one `{ required, result }` record subtree.
fn read_record(repo: &gix::Repository, name: &str, oid: &gix::ObjectId) -> Result<Record, String> {
    let mut required: Option<serde_json::Map<String, serde_json::Value>> = None;
    let mut result: Option<String> = None;
    for entry in read_tree(repo, oid)? {
        let field = String::from_utf8_lossy(&entry.filename).into_owned();
        match field.as_str() {
            "required" => {
                let blob = read_blob(repo, &entry.oid)?;
                let v: serde_json::Value = serde_json::from_slice(&blob)
                    .map_err(|e| format!("required is not JSON: {e}"))?;
                let map = v
                    .as_object()
                    .ok_or_else(|| "required is not a JSON object".to_string())?;
                // Values must be oid strings — the server matches by oid equality.
                for (k, val) in map {
                    if !val.is_string() {
                        return Err(format!("required[{k:?}] is not a string oid"));
                    }
                }
                required = Some(map.clone());
            }
            "result" => {
                let kind = match entry.mode.kind() {
                    gix::objs::tree::EntryKind::Tree => "tree",
                    gix::objs::tree::EntryKind::Commit => "commit",
                    _ => "blob",
                };
                result = Some(format!("{kind} {}", entry.oid));
            }
            _ => {}
        }
    }
    Ok(Record {
        name: name.to_string(),
        required: required.ok_or_else(|| "record has no `required`".to_string())?,
        result: result.ok_or_else(|| "record has no `result`".to_string())?,
    })
}

/// An owned tree entry (name/mode/oid), so it outlives the fetched object bytes.
struct Entry {
    filename: Vec<u8>,
    mode: gix::objs::tree::EntryMode,
    oid: gix::ObjectId,
}

fn read_tree(repo: &gix::Repository, oid: &gix::ObjectId) -> Result<Vec<Entry>, String> {
    let object = repo
        .find_object(*oid)
        .map_err(|e| format!("object {oid} not found: {e}"))?;
    if object.kind != gix::object::Kind::Tree {
        return Err(format!("{oid} is a {}, not a tree", object.kind));
    }
    let tree = gix::objs::TreeRef::from_bytes(&object.data, gix::hash::Kind::Sha1)
        .map_err(|e| format!("malformed tree {oid}: {e}"))?;
    Ok(tree
        .entries
        .iter()
        .map(|e| Entry {
            filename: e.filename.to_vec(),
            mode: e.mode,
            oid: e.oid.to_owned(),
        })
        .collect())
}

fn read_blob(repo: &gix::Repository, oid: &gix::ObjectId) -> Result<Vec<u8>, String> {
    let object = repo
        .find_object(*oid)
        .map_err(|e| format!("object {oid} not found: {e}"))?;
    if object.kind != gix::object::Kind::Blob {
        return Err(format!("{oid} is a {}, not a blob", object.kind));
    }
    Ok(object.data.clone())
}
