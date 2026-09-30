//! The actor wrapper (design/actors.md): run an inner `(state, message) ->
//! (state', reply)` request against state kept on a Git branch, and publish the
//! new state with a compare-and-swap push.
//!
//! `Q = actor { state-ref, inner, nonce, message }` has two positions:
//!
//! - start reads the branch head (`ls-remote`, then a depth-1 `tree:0` fetch of
//!   that one commit), takes the `state/` subtree oid from the head, builds the
//!   inner request and tail-calls it with Q (plus the observed head and the
//!   input state) as the callback;
//! - finish receives the inner's `{state, reply}`. An unchanged state returns
//!   the reply without touching Git; otherwise it commits `{state: <new oid>}`
//!   on the observed head and pushes with `--force-with-lease`. A lost race
//!   fails the request, which is never cached, so the caller retries.
//!
//! Neither position checks the state out: it travels as a tree oid.

use std::path::Path;
use std::process::{Command, ExitCode};

use conversation_protocol::v3::{
    CommitInfo, GitStore, Mode, ObjectStore, Oid, RefUpdate, Signature, TreeEntry,
};
use worker_common::{
    arg, caos, caos_curry, cas_hash, forward, own_args_tree, prepare_request, read_arg, run_worker,
    run_request_then, scratch, Arg,
};

const STATE_ENTRY: &str = "state";
const NO_HEAD: &str = "none";
const GIT_DIR: &str = "/tmp/actor-git";

fn main() -> ExitCode {
    run_worker("actor", run)
}

fn run() -> Result<(), String> {
    let state_ref = read_arg("state-ref")?;
    if !state_ref.starts_with("refs/heads/actors/") || state_ref.contains("..") {
        return Err(format!(
            "state-ref {state_ref:?} must be under refs/heads/actors/"
        ));
    }
    if Path::new(&arg("result")).exists() {
        finish(&state_ref)
    } else {
        start(&state_ref)
    }
}

fn server_url() -> Result<String, String> {
    let url = std::env::var("CAOS_SERVER_URL").map_err(|_| "CAOS_SERVER_URL not set".to_string())?;
    Ok(url.trim_end_matches('/').to_string())
}

fn git(args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(GIT_DIR)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// A bare scratch repository whose origin is the server and which treats it as
/// a promisor remote, so objects that exist only on the server (the actor's
/// state trees) are "promised" and are neither downloaded nor re-sent.
fn promisor_store() -> Result<GitStore, String> {
    let store = GitStore::scratch("actor-git", &server_url()?)?;
    git(&["config", "core.repositoryformatversion", "1"])?;
    git(&["config", "extensions.partialClone", "origin"])?;
    git(&["config", "remote.origin.promisor", "true"])?;
    git(&["config", "remote.origin.partialclonefilter", "tree:0"])?;
    Ok(store)
}

/// Fetch exactly one object from the server through the promisor filter: a
/// commit arrives alone (depth 1, no trees), a tree arrives without its
/// children. Everything it references is then "promised", which is what lets a
/// later push traverse it without downloading the rest.
fn fetch_one(oid: &Oid, shallow: bool) -> Result<(), String> {
    let mut args = vec!["fetch", "--quiet", "--no-tags", "--no-write-fetch-head"];
    // A shallow repository cannot push (the server refuses shallow pushes), so
    // only start, which merely reads, may cut the history off.
    if shallow {
        args.push("--depth=1");
    }
    args.extend(["--filter=tree:0", "origin", oid.as_str()]);
    git(&args)
}

/// The `state/` subtree oid of `head`, reading only the commit and its root tree.
fn state_of(store: &GitStore, head: &Oid) -> Result<Option<Oid>, String> {
    fetch_one(head, true)?;
    let commit = store.read_commit(head).map_err(String::from)?;
    let root = store.read_tree(&commit.tree).map_err(String::from)?;
    Ok(root
        .into_iter()
        .find(|entry| entry.name == STATE_ENTRY)
        .map(|entry| entry.oid))
}

/// The oid of the empty tree, as a CAS object (so it can be bound by path).
fn empty_state() -> Result<String, String> {
    let dir = scratch("actor-empty-state")?;
    caos(["put", worker_common::path(&dir), "/cas/empty-state"])?;
    cas_hash("/cas/empty-state")
}

fn start(state_ref: &str) -> Result<(), String> {
    let store = promisor_store()?;
    let head = store.read_ref(state_ref)?;
    let state = match &head {
        Some(head) => state_of(&store, head)?,
        None => None,
    };
    let (state_path, state_oid) = match state {
        Some(oid) => {
            caos(["get-hash", oid.as_str(), "/cas/state"])?;
            ("/cas/state", oid.to_string())
        }
        None => ("/cas/empty-state", empty_state()?),
    };
    if state_path == "/cas/empty-state" && !Path::new(state_path).exists() {
        return Err("empty state was not staged".to_string());
    }

    let request = prepare_request(
        Arg::Path(&arg("inner")),
        &[
            ("state", Arg::Path(state_path)),
            ("message", Arg::Path(&arg("message"))),
        ],
    )?;

    // The callback is this same Q, carrying what finish needs to publish.
    let q = own_args_tree()?;
    let head_text = head.as_ref().map(Oid::as_str).unwrap_or(NO_HEAD);
    let callback = caos_curry(
        Arg::Hash(&q),
        &[
            ("head", Arg::Lit(head_text)),
            ("old-state", Arg::Lit(&state_oid)),
        ],
    )?;
    run_request_then(&request, Some(Arg::Hash(&callback)))
}

fn finish(state_ref: &str) -> Result<(), String> {
    let result = arg("result");
    // List the result's children as hash-tagged entries; nothing is downloaded.
    caos(["get", &result])?;
    let new_state = cas_hash(&format!("{result}/{STATE_ENTRY}"))?;
    let old_state = read_arg("old-state")?;
    if new_state != old_state {
        let head = match read_arg("head")?.as_str() {
            NO_HEAD => None,
            oid => Some(Oid::parse(oid, "head")?),
        };
        publish(state_ref, head, Oid::parse(&new_state, "new state")?)?;
    }
    forward(&format!("{result}/reply"), "/cas/out")
}

fn publish(state_ref: &str, head: Option<Oid>, new_state: Oid) -> Result<(), String> {
    let mut store = promisor_store()?;
    // This container is not start's, so the scratch repo holds nothing: bring in
    // the parent commit and the new state's root tree, one object each, so the
    // push can traverse the new commit without reading the state's closure.
    if let Some(head) = &head {
        fetch_one(head)?;
    }
    fetch_one(&new_state)?;
    let tree = store
        .write_tree(&[TreeEntry {
            name: STATE_ENTRY.to_string(),
            mode: Mode::Tree,
            oid: new_state,
        }])
        .map_err(String::from)?;
    let identity = Signature {
        name: "actor".to_string(),
        email: "actor@caos".to_string(),
        time: 0,
        offset: "+0000".to_string(),
    };
    let candidate = store
        .write_commit(&CommitInfo {
            tree,
            parents: head.iter().cloned().collect(),
            author: identity.clone(),
            committer: identity,
            extra_headers: Vec::new(),
            message: b"actor state\n".to_vec(),
        })
        .map_err(String::from)?;

    let push_error = match store.push(&[RefUpdate {
        refname: state_ref.to_string(),
        expected: head.clone(),
        new: Some(candidate.clone()),
    }]) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    // Ambiguous failure: re-read the ref to learn what actually happened.
    match store.read_ref(state_ref) {
        Ok(Some(observed)) if observed == candidate => Ok(()),
        Ok(observed) if observed == head => Err(format!("pushing {state_ref}: {push_error}")),
        Ok(_) => Err(format!("lost the race for {state_ref}: {push_error}")),
        Err(read_error) => Err(format!(
            "pushing {state_ref} failed ({push_error}); rereading it also failed: {read_error}"
        )),
    }
}
