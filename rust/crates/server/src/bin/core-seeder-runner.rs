//! caos core-seeder-runner: the bootstrap answerer (design/caos-expr.md,
//! Phase 3, "core-seeder-runner").
//!
//! The irreducible core (`flake-builder`, and later `runner`/`cargo`/`rustc`/
//! `deep-deps`) cannot be built by the machinery it *is*: forming
//! `flake-builder`'s image by running `flake-builder` is a cycle. So
//! `bootstrap`/`build-builtins.sh` hand-builds each core artifact and prints a
//! tree of **seed records**, one per item, which `stack/serve` hands straight
//! to this process as `CAOS_SEED_TREE`:
//!
//! ```text
//! <CAOS_SEED_TREE> -> tree {
//!   <name> -> tree {
//!     required -> blob (JSON: { "<argname>": "<oid>", ... })
//!     result   -> the pre-built object (a tree for an image, a blob otherwise)
//!   }
//! }
//! ```
//!
//! BY VALUE, and it is worth saying why, because this used to be a ref
//! (`refs/caos/seed`) that this process polled every five seconds. A ref is a
//! mutable name: two stacks sharing a server repo overwrite each other's
//! records — same `required`, different `result` is exactly two builds of caos,
//! and the loser then answers with the winner's binaries in silence. Polling
//! also meant a 0-5s window on every bring-up where the core had no answerer,
//! and a warm repo meant registering the PREVIOUS run's records first and
//! swapping later. The publisher and this reader are two steps of one script,
//! so there was never anything for a name to buy.
//!
//! This runner reads those records once and long-polls the server's
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
//! `resolve_expr_base` passes straight through with **no dispatch**, so
//! forming its key runs nothing and this runner just answers a static set of
//! records. The live answerer arrives with the first item that runs another
//! (cargo → flake-builder).
//!
//! Configuration (env):
//!   `CAOS_SERVER_URL`  the server to poll (default `http://127.0.0.1`)
//!   `CAOS_GIT_DIR`     the server's git repo, read for the seed records
//!                      (required — the records live there, colocated)
//!   `CAOS_SEED_TREE`   the tree of seed records to answer from, as printed by
//!                      build-builtins.sh (required; no default, because a
//!                      seeder with nothing to seed is a stack whose core
//!                      cannot be built at all)
//!   `CAOS_RUNNER_TOKEN` bearer token, if the server requires one

use std::time::Duration;

/// How long each poll hangs before re-polling (a reconnect cadence; a seeder
/// never idles out).
///
/// It used to also bound HOW STALE A PARKED POLL COULD BE, because records
/// could be republished under this process. They cannot any more — the seed
/// tree is fixed for this seeder's whole life (see [`main`]) — so this is a
/// reconnect cadence and nothing else. Kept short anyway: it is what
/// `CAOS_SEEDED_GRACE_SECS` has to clear before the server may call a parked
/// seeder a permanent mismatch (server/runner.rs `seeded_verdict`).
const POLL_TTL: Duration = Duration::from_secs(20);

/// Backoff after a failed poll (server not ready).
const RETRY: Duration = Duration::from_secs(2);

struct Config {
    server_url: String,
    git_dir: String,
    seed_tree: String,
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
    // THE SEED TREE IS PASSED BY VALUE, and that is the whole shape of this
    // program. `stack/serve` runs build-builtins, which prints the tree it just
    // published, and starts this with that oid — so the records are fixed for
    // this process's life, read once, and cannot be replaced under it.
    //
    // It used to read `refs/caos/seed` on a 5s timer instead, and everything
    // this file no longer contains was the cost of that: a rescan loop, a
    // shared record map behind a mutex, a re-read per poll iteration and again
    // after every claim, a withdraw path for names that vanished, and a
    // `now answers X (was Y)` log line for the moment a republish landed
    // underneath a parked poll. All of it existed because a mutable ref is a
    // channel any number of publishers can write, on nobody's schedule.
    //
    // What that cost, concretely, before it was hardened: TWO live answerers
    // for flake-builder and runner, THREE for cargo and rustc, handing out
    // different results by coin flip, which alternated the builder image and
    // made the suite recompile every std tool about half the time (35s against
    // 67-85s). And after hardening it still left a 0-5s window on every stack
    // bring-up where the core had no registered answerer at all.
    let seed_tree = match std::env::var("CAOS_SEED_TREE") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            eprintln!(
                "core-seeder-runner: CAOS_SEED_TREE is required — the tree of seed records to \
                 answer from, as printed by build-builtins.sh. A seeder with nothing to seed is \
                 not a degraded stack, it is a stack whose core cannot be built at all, so this \
                 refuses rather than idling."
            );
            std::process::exit(2);
        }
    };
    let config = Config {
        server_url: env_or("CAOS_SERVER_URL", "http://127.0.0.1"),
        git_dir,
        seed_tree,
        token: std::env::var("CAOS_RUNNER_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
    };
    eprintln!(
        "core-seeder-runner: server {}, seed tree {} in {}",
        config.server_url, config.seed_tree, config.git_dir
    );

    // Read once. A failure here is fatal for the same reason an absent
    // CAOS_SEED_TREE is: there is no later attempt that could do better, and a
    // seeder that comes up answering nothing turns every seeded job into a
    // silent park and then a 503 that blames capacity.
    let records = match read_records(&config) {
        Ok(records) if !records.is_empty() => records,
        Ok(_) => {
            eprintln!(
                "core-seeder-runner: seed tree {} holds no records",
                config.seed_tree
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!(
                "core-seeder-runner: reading seed tree {}: {e}",
                config.seed_tree
            );
            std::process::exit(1);
        }
    };

    // One thread per record, each parked on its own long poll. They never
    // exit — a seeder is standing capacity, not a one-shot answer — so the last
    // one runs on this thread rather than being joined.
    let mut spawned = Vec::new();
    for rec in records {
        eprintln!(
            "core-seeder-runner: answering {} -> {}",
            rec.name, rec.result
        );
        let server = config.server_url.clone();
        let token = config.token.clone();
        spawned.push(std::thread::spawn(move || {
            poll_loop(&server, token.as_deref(), &rec)
        }));
    }
    for t in spawned {
        let _ = t.join();
    }
}

/// One RECORD's poll thread: claim a matching job, post that record's result,
/// repeat. Never exits — a seeder is standing capacity, not a one-shot answer.
///
/// The record is owned by this thread and never changes. That is what the seed
/// tree being passed by value buys: there is no shared map, no re-read after a
/// claim, and no window in which what we polled on and what we answer with can
/// differ.
fn poll_loop(server: &str, token: Option<&str>, rec: &Record) {
    let name = &rec.name;
    loop {
        match poll(server, token, rec) {
            Ok(Some((req, nonce))) => {
                if let Err(e) = post_result(server, token, &req, &nonce, &rec.result) {
                    eprintln!("core-seeder-runner: {name}: posting result: {e}");
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

/// Read the seed records out of `CAOS_SEED_TREE`, a tree of
/// `<name> -> {required, result}` that build-builtins.sh has already pushed to
/// this repo.
///
/// BY OID, not through a ref. The publisher and this reader are two steps of
/// one script (`stack/serve`), so the tree can be handed over directly — which
/// means there is no name for a second publisher to take over, nothing to poll
/// for, and no moment where the records on disk are not the ones this process
/// was told about. An unreadable tree is fatal at the call site: the objects
/// were pushed seconds ago by the same script, so a miss is a broken stack, not
/// a race to wait out.
fn read_records(config: &Config) -> Result<Vec<Record>, String> {
    let repo = gix::open(&config.git_dir).map_err(|e| format!("open {}: {e}", config.git_dir))?;
    let root = gix::ObjectId::from_hex(config.seed_tree.as_bytes())
        .map_err(|e| format!("{} is not an object id: {e}", config.seed_tree))?;
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
