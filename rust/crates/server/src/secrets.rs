//! Secrets during a run (SPEC.md, "Secrets"): what a request's
//! SecretReaderKeys hold, which images evaluation granted them to, and
//! injection at dispatch.
//!
//! A grant is decided where evaluation applies a node's `.caos-expr`, from the
//! node's ORIGINS (`caos_eval::Origin`). A granted image is marked with
//! `secret-hash` and recorded; dispatch injects only into a job whose ArgTree
//! is a superset of a recorded image. The record is what proves the image came
//! from this server's evaluation: `secret-hash` alone is visible to anyone who
//! has seen a granted run.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use caos_eval::{EvalHost, Origin};
use caos_world::secrets::Reader;

use crate::secret_store::Stored;
use crate::{Config, HttpError};

/// The secrets a run carries, fixed at admission and passed to every sub-run.
#[derive(Clone, Default)]
pub(crate) struct Context {
    stored: Arc<Vec<Stored>>,
    /// The resolved trees and the conversation: what every record is keyed by,
    /// so a push or another conversation sees none of this run's grants.
    scope: String,
    conversation: Option<String>,
}

impl Context {
    /// Resolve a request's SecretReaderKeys (space-separated) and conversation.
    pub(crate) fn admit(
        config: &Config,
        readers: &str,
        conversation: Option<&str>,
    ) -> Result<Context, HttpError> {
        let keys: Vec<String> = readers.split_whitespace().map(str::to_string).collect();
        if keys.is_empty() {
            return Ok(Context::default());
        }
        let (stored, trees) =
            crate::secret_store::load(config, &keys).map_err(|e| HttpError::new(400, e))?;
        let conversation = conversation.filter(|c| !c.is_empty()).map(str::to_string);
        Ok(Context {
            scope: format!(
                "{}|{}",
                trees.join(","),
                conversation.as_deref().unwrap_or("")
            ),
            stored: Arc::new(stored),
            conversation,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.stored.is_empty()
    }
}

/// Every `(root, path)` a tree has been reached at, by tree oid: a start tree,
/// a `:@@=` result, a value a `.caos-expr` produced, and every subtree of each.
/// Equal oids are equal content, so a copy — a `DEEP-DEPS/<name>` mount is one —
/// carries the origin of what it was copied from. Facts about content, so
/// shared by every run.
static KNOWN_ORIGINS: Mutex<Option<HashMap<String, Vec<Origin>>>> = Mutex::new(None);

/// Bounds [`KNOWN_ORIGINS`]; forgetting only means a later grant misses.
const KNOWN_ORIGINS_CAP: usize = 1_000_000;

/// The values whose subtrees are already in [`KNOWN_ORIGINS`], with the
/// origins they were indexed under.
type Indexed = HashSet<(String, Vec<Origin>)>;
static INDEXED: Mutex<Option<Indexed>> = Mutex::new(None);

/// Record `(T, P/q)` for every subtree at `q` under `tree`, for each origin
/// `(T, P)` of `tree` itself.
fn index_subtrees(config: &Config, tree: &str, origins: &[Origin]) -> Result<(), String> {
    {
        let mut guard = INDEXED.lock().unwrap_or_else(|e| e.into_inner());
        let indexed = guard.get_or_insert_with(HashSet::new);
        if indexed.len() >= KNOWN_ORIGINS_CAP {
            indexed.clear();
        }
        if !indexed.insert((tree.to_string(), origins.to_vec())) {
            return Ok(());
        }
    }
    let repo = config.repo.to_thread_local();
    let root = gix::ObjectId::from_hex(tree.as_bytes()).map_err(|e| format!("{tree}: {e}"))?;
    if repo
        .find_header(root)
        .map_err(|e| format!("{tree}: {e}"))?
        .kind()
        != gix::object::Kind::Tree
    {
        return Ok(());
    }
    let mut pending = vec![(root, String::new())];
    while let Some((oid, rel)) = pending.pop() {
        let object = repo.find_object(oid).map_err(|e| format!("{oid}: {e}"))?;
        let decoded = object.try_into_tree().map_err(|e| format!("{oid}: {e}"))?;
        for entry in decoded.decode().map_err(|e| format!("{oid}: {e}"))?.entries {
            if !entry.mode.is_tree() {
                continue;
            }
            let path = if rel.is_empty() {
                entry.filename.to_string()
            } else {
                format!("{rel}/{}", entry.filename)
            };
            for origin in origins {
                record_origin(
                    &entry.oid.to_string(),
                    Origin {
                        root: origin.root.clone(),
                        path: if origin.path.is_empty() {
                            path.clone()
                        } else {
                            format!("{}/{path}", origin.path)
                        },
                    },
                );
            }
            pending.push((entry.oid.to_owned(), path));
        }
    }
    Ok(())
}

pub(crate) fn record_origin(oid: &str, origin: Origin) {
    let mut guard = KNOWN_ORIGINS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() >= KNOWN_ORIGINS_CAP {
        map.clear();
    }
    let origins = map.entry(oid.to_string()).or_default();
    if !origins.contains(&origin) {
        origins.push(origin);
    }
}

/// A walk's start tree, which is its own origin: every subtree is at its path.
pub(crate) fn index_root(config: &Config, context: &Context, tree: &str) {
    if context.is_empty() {
        return;
    }
    let origin = Origin {
        root: tree.to_string(),
        path: String::new(),
    };
    if let Err(e) = index_subtrees(config, tree, &[origin]) {
        eprintln!("secrets: cannot index {tree}: {e}");
    }
}

/// What a `:@@=` resolution produced, at its locator's `(rev^{tree}, dir)`.
pub(crate) fn record_mount(config: &Config, oid: &str, origin: Origin) {
    record_origin(oid, origin.clone());
    if let Err(e) = index_subtrees(config, oid, &[origin]) {
        eprintln!("secrets: cannot index {oid}: {e}");
    }
}

pub(crate) fn origins_of(oid: &str) -> Vec<Origin> {
    let guard = KNOWN_ORIGINS.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .and_then(|map| map.get(oid).cloned())
        .unwrap_or_default()
}

/// A granted image's entries and the names granted, by scope.
type Records = HashMap<String, Vec<(BTreeMap<String, String>, Vec<String>)>>;
static RECORDS: Mutex<Option<Records>> = Mutex::new(None);

/// [`EvalHost::evaluated`] for the server: mark and record `value` when one of
/// the node's origins matches a reader of the run's secrets.
pub(crate) fn evaluated(
    config: &Config,
    host: &dyn EvalHost,
    context: &Context,
    origins: &[Origin],
    value: (String, String),
) -> Result<(String, String), String> {
    if context.is_empty() {
        return Ok(value);
    }
    if value.0 == "tree" {
        index_subtrees(config, &value.1, origins)?;
    }
    let granted: Vec<&Stored> = context
        .stored
        .iter()
        .filter(|secret| {
            secret.readers.iter().any(|reader| {
                origins
                    .iter()
                    .any(|origin| matches(config, context, reader, origin))
            })
        })
        .collect();
    if granted.is_empty() {
        return Ok(value);
    }
    let mut names: Vec<String> = granted.iter().map(|s| s.name.clone()).collect();
    if value.0 != "tree" {
        eprintln!(
            "secrets: {names:?} match a node that evaluates to a {}, which cannot carry a grant",
            value.0
        );
        return Ok(value);
    }
    // A value built on a granted image already carries that image's mark —
    // one `secret-hash` per ArgTree, so it is replaced by the union.
    let mut image = value.1.clone();
    let existing =
        crate::compute::image_entries(config, &image).map_err(|e| e.message().to_string())?;
    if existing.contains_key(caos_world::SECRET_HASH_ARG) {
        names.extend(recorded_names(context, &existing));
        names.sort();
        names.dedup();
        image = crate::compute::unbind(config, &image, caos_world::SECRET_HASH_ARG)
            .map_err(|e| e.message().to_string())?;
    }
    let pairs: Vec<(&str, &str)> = context
        .stored
        .iter()
        .filter(|s| names.contains(&s.name))
        .map(|s| (s.name.as_str(), s.entropy.as_str()))
        .collect();
    let digest = blob_oid(&caos_world::secret_hash_material(&pairs));
    let entry = gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Blob.into(),
        filename: caos_world::SECRET_HASH_ARG.into(),
        oid: host.post_object("blob", digest.as_bytes())?,
    };
    let marked = caos_eval::curry(host, &image, vec![entry])?.to_string();
    let entries =
        crate::compute::image_entries(config, &marked).map_err(|e| e.message().to_string())?;
    let mut guard = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    let records = guard
        .get_or_insert_with(HashMap::new)
        .entry(context.scope.clone())
        .or_default();
    if !records.iter().any(|(e, _)| *e == entries) {
        records.push((entries, names.clone()));
    }
    eprintln!("secrets: granted {names:?} to {marked}");
    Ok(("tree".to_string(), marked))
}

/// The secrets a job may read: those of every image evaluation granted in this
/// run's scope whose entries the job's ArgTree contains. Returns (name, value).
pub(crate) fn grant(
    context: &Context,
    arg_entries: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let names = granted_names(context, arg_entries);
    let out: Vec<(String, String)> = context
        .stored
        .iter()
        .filter(|s| names.contains(&s.name))
        .map(|s| (s.name.clone(), s.value.clone()))
        .collect();
    for (name, _) in &out {
        eprintln!("secret {name}: granted to this job");
    }
    out
}

/// The names [`grant`] would inject, sorted.
/// The names recorded for exactly the image whose entries are `entries`.
fn recorded_names(context: &Context, entries: &BTreeMap<String, String>) -> Vec<String> {
    let guard = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .and_then(|r| r.get(&context.scope))
        .into_iter()
        .flatten()
        .filter(|(recorded, _)| recorded == entries)
        .flat_map(|(_, names)| names.iter().cloned())
        .collect()
}

pub(crate) fn granted_names(
    context: &Context,
    arg_entries: &BTreeMap<String, String>,
) -> Vec<String> {
    if context.is_empty() {
        return Vec::new();
    }
    let guard = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(records) = guard.as_ref().and_then(|r| r.get(&context.scope)) else {
        return Vec::new();
    };
    let mut names = HashSet::new();
    for (entries, granted) in records {
        if entries.iter().all(|(k, v)| arg_entries.get(k) == Some(v)) {
            names.extend(granted.iter().cloned());
        }
    }
    let mut names: Vec<String> = names.into_iter().collect();
    names.sort();
    names
}

fn matches(config: &Config, context: &Context, reader: &Reader, origin: &Origin) -> bool {
    let outcome = match reader {
        Reader::Locator { locator, since } => {
            match_locator(config, locator, since.as_deref(), origin)
        }
        Reader::Conversation { path, conversation } => {
            if context.conversation.as_deref() != Some(conversation.as_str()) {
                return false;
            }
            match_conversation(config, conversation, path, origin)
        }
    };
    outcome.unwrap_or_else(|e| {
        eprintln!("secrets: a reader could not be checked, so it grants nothing: {e}");
        false
    })
}

fn components(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect()
}

fn match_locator(
    config: &Config,
    locator: &str,
    since: Option<&str>,
    origin: &Origin,
) -> Result<bool, String> {
    let git_ref = git_locator::parse_git_ref(locator)?;
    if components(git_ref.dir.as_deref().unwrap_or("")) != components(&origin.path) {
        return Ok(false);
    }
    Ok(crate::grant_history::allowed_roots(config, &git_ref, since)?.contains(&origin.root))
}

/// `path` in the conversation's head tree, evaluated the way the conversation
/// evaluates it: from the root of whichever tree along the path the walk
/// started at, a source tree's gitlink included.
fn match_conversation(
    config: &Config,
    conversation: &str,
    path: &str,
    origin: &Origin,
) -> Result<bool, String> {
    let parts = components(path);
    let rest = components(&origin.path);
    if rest.len() > parts.len() || parts[parts.len() - rest.len()..] != rest[..] {
        return Ok(false);
    }
    let prefix = &parts[..parts.len() - rest.len()];
    let repo = config.repo.to_thread_local();
    let refname = conversation_protocol::v3::refs::head_ref(conversation)?;
    let Ok(mut reference) = repo.find_reference(refname.as_str()) else {
        return Ok(false);
    };
    let mut tree = reference
        .peel_to_commit()
        .map_err(|e| format!("{refname}: {e}"))?
        .tree_id()
        .map_err(|e| format!("{refname}: {e}"))?
        .detach();
    for name in prefix {
        let object = repo.find_object(tree).map_err(|e| format!("{tree}: {e}"))?;
        let Ok(decoded) = object.try_into_tree() else {
            return Ok(false);
        };
        let Some(entry) = decoded
            .decode()
            .map_err(|e| format!("{tree}: {e}"))?
            .entries
            .iter()
            .find(|e| e.filename == name.as_bytes())
            .map(|e| (e.mode, e.oid.to_owned()))
        else {
            return Ok(false);
        };
        tree = if entry.0.is_commit() {
            repo.find_object(entry.1)
                .map_err(|e| format!("{}: {e}", entry.1))?
                .try_into_commit()
                .map_err(|e| format!("{}: {e}", entry.1))?
                .tree_id()
                .map_err(|e| format!("{}: {e}", entry.1))?
                .detach()
        } else {
            entry.1
        };
    }
    Ok(tree.to_string() == origin.root)
}

fn blob_oid(bytes: &[u8]) -> String {
    gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
        .expect("hashing bytes")
        .to_string()
}
