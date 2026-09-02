//! `.caos-expr` evaluation, client side (design/caos-expr.md).
//!
//! A `.caos-expr` file makes the directory it sits in *evaluable*: instead of
//! being taken verbatim, the directory's contents are computed by running the
//! expression the file holds. The WALK itself lives in the shared `caos-eval`
//! crate, generic over a `caos_eval::EvalHost` so it runs identically here
//! (blocking at top level — a client holds no worker slot) and in the server
//! (blocking a request thread to resolve an `eval` continuation). One walk, two
//! backends: that is what makes a worker's server-side `eval-path-then` return
//! the byte-identical object a client `eval-path` would build.
//!
//! This module is the CLIENT backend — [`ClientEvalHost`], which dispatches a
//! `run` through `request_compute`, marks curries with the caller's secret store
//! (design/secrets.md), and fetches `:@@=` locators — plus the CLI and
//! resolution entry points that used to hold the walk.
//!
//! **`:@@=` is client-only, and not because of a sandbox.** A locator carries a
//! mandatory commit sha, so it is perfectly deterministic; the point is that the
//! ArgTree IS the cache key, so a locator must become an oid *before* the
//! request is formed — otherwise the URL sits inside the key and two consumers
//! pinning the same rev through different URLs (a fork, a mirror, ssh vs https)
//! key identical content differently. `caos_eval::EvalHost::resolve_remote`
//! therefore refuses by default, and only this host overrides it.
//!
//! The grammar, the here-string form, the `$CAOS_EXPR` binding and the
//! worker-vs-data rule are documented on `caos_eval` itself.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use caos_eval::MemoKind;
use gix::objs::tree::Entry;

use super::{
    assemble_arg_tree, build_secret_store, fetch_tree_entries, mark_arg_tree, post_object,
    post_tree, request_compute, resolve_remote_arg, secret_store_header, store_key, ClientSecret,
    Transport,
};

/// The shared walk's kind/mode helper, re-exported so the rest of the client
/// keeps naming it through `eval::` — the same function the server uses.
pub(crate) use caos_eval::mode_of_kind;

/// A process-wide memo whose keys are CONTENT.
///
/// Every key stored through this is built from object hashes — a tree and a path
/// within it, a commit sha — so an entry cannot go stale: the same key names the
/// same bytes for the life of the world. That is the whole licence for a global
/// here. A cache keyed on a NAME would need invalidation; this one has nothing
/// to invalidate, and a long-lived process (the TUI) accumulates one small entry
/// per distinct object it evaluated.
///
/// The lock is held across the map access and never across the work, so two
/// threads racing on a cold key both compute and both insert — the same answer,
/// because the key is the content. That costs a duplicated round trip once;
/// holding it across the evaluation would serialize every evaluation in the
/// process behind whichever one is dispatching a run.
pub(crate) struct Memo<V>(OnceLock<Mutex<HashMap<String, V>>>);

impl<V: Clone> Memo<V> {
    pub(crate) const fn new() -> Self {
        Self(OnceLock::new())
    }

    fn map(&self) -> &Mutex<HashMap<String, V>> {
        self.0.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(crate) fn get(&self, key: &str) -> Option<V> {
        // A poisoned lock means some OTHER thread panicked; the map is still
        // consistent, because nothing fallible runs while it is held. Taking the
        // inner map is therefore recovery, not a swallowed error.
        self.map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    pub(crate) fn put(&self, key: String, value: V) {
        self.map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, value);
    }
}

/// The walk's memos, one per [`MemoKind`], scoped by the caller's secret store:
/// `<store>\0<content key>` → `(kind, oid)`.
///
/// The SCOPE is this host's contribution and the reason the memo lives here
/// rather than in `caos-eval`: a store's readers change what a `curry`
/// evaluates to (`mark_curry`), and the client is the only host that has one.
/// `CAOS_SALT` is not in the key because [`crate::run_salt`] reads the
/// environment, which is fixed for the process — two different salts never meet
/// in one memo. The server keeps no memo here at all: its salt and stack vary
/// per request, so it opts out (the trait default) until it has a key that says
/// so.
static NODE_MEMO: Memo<(String, String)> = Memo::new();
static PATH_MEMO: Memo<(String, String)> = Memo::new();

fn memo_for(kind: MemoKind) -> &'static Memo<(String, String)> {
    match kind {
        MemoKind::Node => &NODE_MEMO,
        MemoKind::Path => &PATH_MEMO,
    }
}

/// The CLIENT `caos_eval::EvalHost`: CAS over a `Transport`, a `run` dispatched
/// by `request_compute` (blocking, at top level), curries marked with the
/// caller's secret store (a no-op without secrets, so the common path is
/// byte-identical to the server's), and `:@@=` locators fetched — the one
/// capability no other host has.
struct ClientEvalHost<'a> {
    t: &'a dyn Transport,
    store: &'a [ClientSecret],
}

impl caos_eval::EvalHost for ClientEvalHost<'_> {
    fn get_object(&self, oid: &str) -> Result<(String, Vec<u8>), String> {
        self.t.get_object(oid)
    }
    fn post_object(&self, kind: &str, bytes: &[u8]) -> Result<gix::ObjectId, String> {
        post_object(self.t, kind, bytes)
    }
    fn fetch_tree_entries(&self, tree: &str) -> Result<Option<Vec<Entry>>, String> {
        fetch_tree_entries(self.t, tree)
    }
    fn post_tree(&self, entries: Vec<Entry>) -> Result<gix::ObjectId, String> {
        post_tree(self.t, entries)
    }
    fn dispatch(&self, image: &str, entries: Vec<Entry>) -> Result<(String, String), String> {
        // A `run` marks via `assemble_arg_tree` and must carry the store to the
        // server (to inject and pass the double-check), exactly like `caos-cli
        // run`.
        let arg_tree = assemble_arg_tree(self.t, image, entries, self.store)?;
        let server = self.t.server_url()?;
        request_compute(&server, &arg_tree, &secret_store_header(self.store))
    }
    fn mark_curry(&self, oid: &str) -> Result<String, String> {
        mark_arg_tree(self.t, self.store, oid)
    }
    fn resolve_remote(
        &self,
        value: &str,
    ) -> Result<(gix::objs::tree::EntryMode, gix::ObjectId), String> {
        // `dir=` descends through EVALUATION (see `resolve_remote_arg`), so a
        // pinned consumer reaches `dir=std/<x>` exactly as caos reaches its own
        // entries. It carries its own memo, on the locator.
        resolve_remote_arg(self.t, value, self.store)
    }
    fn memo_get(&self, kind: MemoKind, key: &str) -> Option<(String, String)> {
        memo_for(kind).get(&self.memo_key(key))
    }
    fn memo_put(&self, kind: MemoKind, key: &str, value: &(String, String)) {
        memo_for(kind).put(self.memo_key(key), value.clone());
    }
}

impl ClientEvalHost<'_> {
    /// The walk's content key, scoped by the caller's store — see [`NODE_MEMO`].
    fn memo_key(&self, key: &str) -> String {
        format!("{}\0{key}", store_key(self.store))
    }
}

/// Walk `start_tree` from its root down to `path`, evaluating every `.caos-expr`
/// encountered (each in the tree its parent produced) and returning the final
/// object's `(kind, oid)` — the client entry to `caos_eval::eval_path`. `store`
/// is the caller's secret store, threaded into the host for `run` dispatch and
/// curry marking.
pub(crate) fn eval_path(
    t: &dyn Transport,
    start_tree: &str,
    path: &str,
    store: &[ClientSecret],
) -> Result<(String, String), String> {
    let host = ClientEvalHost { t, store };
    caos_eval::eval_path(&host, start_tree, path)
}

/// Resolve a workspace entry point NAMED BY PATH: evaluate the tracked tree and
/// descend to `path`.
///
/// The sibling below reaches an entry through `DEEP-DEPS/<name>`, which is what
/// a repo that DECLARED the dependency in a root `DEPS` gets. This tree has no
/// root `DEPS` — `DEEP-DEPS` does not exist at its root at all — so a client
/// here names a std entry by its path instead.
///
/// That still evaluates, and evaluating is still not optional: the walk starts
/// at the tree ROOT, so the root `.caos-expr` runs first and deepens the tree,
/// and only then does the descent reach `std/<name>` — in the deepened tree,
/// where its own `DEEP-DEPS/rustc` exists. Naming the raw worktree directory
/// would not work, exactly as the sibling's doc says.
pub fn eval_workspace_path(
    t: &dyn Transport,
    path: &str,
    store: &[ClientSecret],
) -> Result<String, String> {
    let (_, oid) = t
        .ingest_path(".")?
        .ok_or_else(|| "this client cannot ingest the workspace tree".to_string())?;
    let (kind, hash) = eval_path(t, &oid.to_string(), path, store)
        .map_err(|error| format!("resolving {path:?} from the workspace: {error}"))?;
    if kind != "tree" {
        return Err(format!(
            "{path}/.caos-expr evaluates to a {kind}, not an ArgTree"
        ));
    }
    Ok(hash)
}

/// Resolve one of the WORKSPACE's declared entry points: evaluate the tracked
/// tree and descend to `DEEP-DEPS/<name>`.
///
/// This is how a client reaches a tool without an ambient library. The workspace
/// declares what it needs in a `DEPS` file (`./std/llm-step llm-step` here,
/// `./flake-inputs/caos/std/llm-step llm-step` in a repo that mounted caos), the
/// root `.caos-expr` expands that into `DEEP-DEPS/`, and this descends it — the
/// same declaration, the same transform and the same mount names a worker sees.
///
/// Evaluating is not optional. A std entry's expression names its own
/// dependencies by mount (`run --base:@=DEEP-DEPS/rustc …`), and those exist only in
/// the DEEPENED tree — so resolving the raw `std/llm-step` directory out of the
/// worktree cannot work, whatever the path is spelled.
pub fn eval_workspace_dep(t: &dyn Transport, name: &str) -> Result<String, String> {
    eval_workspace_dep_with_store(t, name, &[])
}

/// Resolve a workspace entry point while carrying the caller's secret store
/// through expression evaluation. Conversation setup uses this form so a tool
/// embedded by the llm-step expression keeps its secret-dependent identity in
/// the enclosing turn request.
pub fn eval_workspace_dep_with_store(
    t: &dyn Transport,
    name: &str,
    store: &[ClientSecret],
) -> Result<String, String> {
    let (_, oid) = t
        .ingest_path(".")?
        .ok_or_else(|| "this client cannot ingest the workspace tree".to_string())?;
    eval_path(t, &oid.to_string(), &format!("DEEP-DEPS/{name}"), store)
        .map(|(_kind, hash)| hash)
        .map_err(|error| workspace_dep_error(name, &error))
}

fn workspace_dep_error(name: &str, error: &str) -> String {
    let mut message = format!("resolving {name:?} from the workspace: {error}");
    // A transport, worker, or expression failure does not imply a missing DEPS
    // line. Offer the declaration hint only when the actual tree walk says its
    // deep-deps mount is absent.
    if error.starts_with("eval-path: ")
        && error.contains(" not found in ")
        && (error.contains("\"DEEP-DEPS\"") || error.contains(&format!("{name:?}")))
    {
        message.push_str(&format!(
            "\n  declare it in ./DEPS, e.g. `./std/{name} {name}`"
        ));
    }
    message
}

/// `eval-path [--tree=<oid>] <path>` — evaluate the `.caos-expr` files from the
/// root of the tree down to `<path>` and print the resulting object's
/// `"<kind> <hash>"`. With no `--tree`, the tracked workspace tree is the start
/// (dirty edits included, like `run-tool`'s `--in:@=.`).
pub fn cli_eval_path(t: &dyn Transport, tree: Option<&str>, path: &str) -> Result<(), String> {
    let start = match tree {
        Some(oid) => {
            let (kind, _) = t.get_object(oid)?;
            if kind != "tree" {
                return Err(format!("--tree={oid} is a {kind}, not a tree"));
            }
            oid.to_string()
        }
        None => {
            let (_, oid) = t
                .ingest_path(".")?
                .ok_or_else(|| "this client cannot ingest the workspace tree".to_string())?;
            oid.to_string()
        }
    };
    // The caller's secret store, resolved once — so eval-path marks the `curry`
    // arg trees it returns, giving their callers per-user isolation, and carries
    // the store to any `run` it dispatches (design/secrets.md). A `:@=` target is
    // NOT marked; see `caos_eval`'s `resolve_expr_path`.
    let store = build_secret_store(t)?;
    let (kind, hash) = eval_path(t, &start, path, &store)?;
    println!("{kind} {hash}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::workspace_dep_error;

    #[test]
    fn workspace_dependency_hint_only_follows_a_missing_mount() {
        let missing = workspace_dep_error(
            "llm-step",
            "eval-path: \"llm-step\" not found in 0123456789abcdef",
        );
        assert!(missing.contains("declare it in ./DEPS"), "{missing}");

        let push = workspace_dep_error("llm-step", "git push failed: bad tree object");
        assert!(!push.contains("declare it in ./DEPS"), "{push}");
    }
}
