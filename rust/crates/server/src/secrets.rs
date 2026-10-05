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
//!
//! Origins and records are kept in redis as well as here, so a restart forgets
//! neither and a cached walk needs nothing replayed. Both are facts — about
//! content, and about a scope that names every input to the grant — so no
//! build's answer differs and they are not namespaced.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use caos_eval::{EvalHost, Origin};
use caos_world::secrets::Reader;

use crate::compute::{fact_get, fact_set, list_append_many, list_read};
use crate::secret_store::Stored;
use crate::{Config, HttpError};

/// The secrets a run carries, fixed at admission and passed to every sub-run.
#[derive(Clone, Default)]
pub(crate) struct Context {
    stored: Arc<Vec<Stored>>,
    /// Each `reader:@@=` locator as matched: a `ref=` resolved to the commit
    /// it named at admission, so one run sees one answer. `None` when it could
    /// not be resolved, which grants nothing.
    pins: Arc<HashMap<String, Option<String>>>,
    /// Every input a grant depends on — the stores' trees, the conversation,
    /// the pinned commits — so a push, a moved branch or another conversation
    /// sees none of this run's records.
    scope: String,
    conversation: Option<String>,
    /// The conversation's head tree, when a `reader:@=` grant names it.
    head: Option<String>,
    /// Which writer this run acts for (design/ref-writers.md). Not a secret
    /// and not in any key; it rides here because this is what reaches every
    /// sub-run.
    writes: crate::ref_writers::Writes,
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
        let mut pins = HashMap::new();
        let mut head = None;
        for reader in stored.iter().flat_map(|s| &s.readers) {
            match reader {
                Reader::Locator { locator, .. } if !pins.contains_key(locator) => {
                    let pinned = crate::grant_history::pin(config, locator).map_err(|e| {
                        eprintln!("secrets: {locator}: {e}; it grants nothing");
                    });
                    pins.insert(locator.clone(), pinned.ok());
                }
                Reader::Conversation {
                    conversation: id, ..
                } if Some(id) == conversation.as_ref() && head.is_none() => {
                    head = conversation_head(config, id).unwrap_or_else(|e| {
                        eprintln!("secrets: conversation {id}: {e}");
                        None
                    });
                }
                _ => {}
            }
        }
        let mut pinned: Vec<&str> = pins.values().flatten().map(String::as_str).collect();
        pinned.sort();
        Ok(Context {
            scope: format!(
                "{}|{}|{}",
                trees.join(","),
                conversation.as_deref().unwrap_or(""),
                pinned.join(",")
            ),
            stored: Arc::new(stored),
            pins: Arc::new(pins),
            conversation,
            head,
            writes: crate::ref_writers::Writes::None,
        })
    }

    pub(crate) fn with_writes(mut self, writes: crate::ref_writers::Writes) -> Context {
        self.writes = writes;
        self
    }

    pub(crate) fn writes(&self) -> &crate::ref_writers::Writes {
        &self.writes
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.stored.is_empty()
    }

    /// What a walk's result depends on besides its tree and path: `None` with
    /// no secrets, since then it depends on nothing more.
    pub(crate) fn walk_key(&self) -> Option<String> {
        (!self.is_empty()).then(|| format!("{}|{}", self.scope, self.head.as_deref().unwrap_or("")))
    }
}

// ---- origins ----------------------------------------------------------------

/// Every `(root, path)` a tree has been reached at, by tree oid: a start tree,
/// a `:@@=` result, a value a `.caos-expr` produced, and every subtree of each.
/// Equal oids are equal content, so a copy — a `DEEP-DEPS/<name>` mount is one —
/// carries the origin of what it was copied from. In front of redis, and an
/// entry here is everything redis holds for that oid.
static KNOWN_ORIGINS: Mutex<Option<HashMap<String, Vec<Origin>>>> = Mutex::new(None);

/// Bounds the in-memory maps; forgetting only means a later redis read.
const MEMORY_CAP: usize = 1_000_000;

fn origins_key(oid: &str) -> String {
    format!("caos:secrets:origins:{oid}")
}

fn encode_origin(origin: &Origin) -> String {
    format!("{}\t{}", origin.root, origin.path)
}

fn decode_origin(text: &str) -> Option<Origin> {
    let (root, path) = text.split_once('\t')?;
    Some(Origin {
        root: root.to_string(),
        path: path.to_string(),
    })
}

/// The known origins of `oid`, from memory or else redis.
pub(crate) fn origins_of(config: &Config, oid: &str) -> Vec<Origin> {
    if let Some(hit) = KNOWN_ORIGINS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|map| map.get(oid).cloned())
    {
        return hit;
    }
    let loaded: Vec<Origin> = list_read(&config.redis_addr, &origins_key(oid))
        .unwrap_or_else(|e| {
            eprintln!("secrets: reading the origins of {oid}: {e}");
            Vec::new()
        })
        .iter()
        .filter_map(|text| decode_origin(text))
        .collect();
    let mut guard = KNOWN_ORIGINS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() >= MEMORY_CAP {
        map.clear();
    }
    let entry = map.entry(oid.to_string()).or_default();
    for origin in loaded {
        if !entry.contains(&origin) {
            entry.push(origin);
        }
    }
    entry.clone()
}

/// Record each `(oid, origin)`, in redis and in memory where memory already
/// holds that oid. An oid it does not hold is left for [`origins_of`] to load
/// whole, so a repeat in redis costs nothing but a duplicate line.
fn record_origins(config: &Config, facts: Vec<(String, Origin)>) {
    let mut new = Vec::new();
    {
        let mut guard = KNOWN_ORIGINS.lock().unwrap_or_else(|e| e.into_inner());
        let map = guard.get_or_insert_with(HashMap::new);
        for (oid, origin) in facts {
            if let Some(known) = map.get_mut(&oid) {
                if known.contains(&origin) {
                    continue;
                }
                known.push(origin.clone());
            }
            new.push((origins_key(&oid), encode_origin(&origin)));
        }
    }
    if let Err(e) = list_append_many(&config.redis_addr, &new) {
        eprintln!("secrets: recording {} origin(s): {e}", new.len());
    }
}

/// The values whose subtrees are recorded, with the origins they were recorded
/// under. In front of a redis marker per entry.
static INDEXED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Record `(T, P/q)` for every subtree at `q` under `tree`, for each origin
/// `(T, P)` of `tree` itself.
fn index_subtrees(config: &Config, tree: &str, origins: &[Origin]) -> Result<(), String> {
    let mut marker = format!("caos:secrets:indexed:{tree}");
    for origin in origins {
        marker.push('\n');
        marker.push_str(&encode_origin(origin));
    }
    let marker = format!("caos:secrets:indexed:{}", blob_oid(marker.as_bytes()));
    {
        let mut guard = INDEXED.lock().unwrap_or_else(|e| e.into_inner());
        let indexed = guard.get_or_insert_with(HashSet::new);
        if indexed.len() >= MEMORY_CAP {
            indexed.clear();
        }
        if !indexed.insert(marker.clone()) {
            return Ok(());
        }
    }
    if matches!(fact_get(&config.redis_addr, &marker), Ok(Some(_))) {
        return Ok(());
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
    let mut facts = Vec::new();
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
                facts.push((
                    entry.oid.to_string(),
                    Origin {
                        root: origin.root.clone(),
                        path: if origin.path.is_empty() {
                            path.clone()
                        } else {
                            format!("{}/{path}", origin.path)
                        },
                    },
                ));
            }
            pending.push((entry.oid.to_owned(), path));
        }
    }
    record_origins(config, facts);
    if let Err(e) = fact_set(&config.redis_addr, &marker, "1") {
        eprintln!("secrets: marking {tree} indexed: {e}");
    }
    Ok(())
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
    record_origins(config, vec![(oid.to_string(), origin.clone())]);
    if let Err(e) = index_subtrees(config, oid, &[origin]) {
        eprintln!("secrets: cannot index {oid}: {e}");
    }
}

// ---- records ----------------------------------------------------------------

/// A granted image's entries and the names granted, by scope. In front of a
/// redis list per scope; a scope present here holds everything redis does.
type Record = (BTreeMap<String, String>, Vec<String>);
static RECORDS: Mutex<Option<HashMap<String, Vec<Record>>>> = Mutex::new(None);

fn records_key(scope: &str) -> String {
    format!("caos:secrets:records:{}", blob_oid(scope.as_bytes()))
}

/// The records of `scope`, loading them from redis the first time.
fn records(config: &Config, scope: &str) -> Vec<Record> {
    if let Some(hit) = RECORDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|r| r.get(scope).cloned())
    {
        return hit;
    }
    let loaded: Vec<Record> = list_read(&config.redis_addr, &records_key(scope))
        .unwrap_or_else(|e| {
            eprintln!("secrets: reading the records of a scope: {e}");
            Vec::new()
        })
        .iter()
        .filter_map(|text| serde_json::from_str(text).ok())
        .collect();
    let mut guard = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() >= MEMORY_CAP {
        map.clear();
    }
    map.entry(scope.to_string()).or_insert(loaded).clone()
}

fn record_grant(config: &Config, scope: &str, record: Record) {
    if records(config, scope).iter().any(|(e, _)| *e == record.0) {
        return;
    }
    let text = serde_json::to_string(&record).expect("a record serializes");
    RECORDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .entry(scope.to_string())
        .or_default()
        .push(record);
    if let Err(e) = list_append_many(&config.redis_addr, &[(records_key(scope), text)]) {
        eprintln!("secrets: recording a grant: {e}");
    }
}

// ---- granting ---------------------------------------------------------------

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
        names.extend(
            records(config, &context.scope)
                .into_iter()
                .filter(|(recorded, _)| *recorded == existing)
                .flat_map(|(_, names)| names),
        );
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
    record_grant(config, &context.scope, (entries, names.clone()));
    eprintln!("secrets: granted {names:?} to {marked}");
    Ok(("tree".to_string(), marked))
}

/// The secrets a job may read: those of every image evaluation granted in this
/// run's scope whose entries the job's ArgTree contains. Returns (name, value).
pub(crate) fn grant(
    config: &Config,
    context: &Context,
    arg_entries: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let names = granted_names(config, context, arg_entries);
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
pub(crate) fn granted_names(
    config: &Config,
    context: &Context,
    arg_entries: &BTreeMap<String, String>,
) -> Vec<String> {
    if context.is_empty() {
        return Vec::new();
    }
    let mut names = HashSet::new();
    for (entries, granted) in records(config, &context.scope) {
        if entries.iter().all(|(k, v)| arg_entries.get(k) == Some(v)) {
            names.extend(granted);
        }
    }
    let mut names: Vec<String> = names.into_iter().collect();
    names.sort();
    names
}

fn matches(config: &Config, context: &Context, reader: &Reader, origin: &Origin) -> bool {
    let outcome = match reader {
        Reader::Locator { locator, since } => match context.pins.get(locator) {
            Some(Some(pinned)) => match_locator(config, pinned, since.as_deref(), origin),
            _ => Ok(false),
        },
        Reader::Conversation { path, conversation } => {
            if context.conversation.as_deref() != Some(conversation.as_str()) {
                return false;
            }
            match &context.head {
                Some(head) => match_conversation(config, head, path, origin),
                None => Ok(false),
            }
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

/// The tree of the conversation's head commit, or `None` with no head yet.
/// `conversation` is its address, `<namespace>/<id>`: an id alone would match
/// a conversation of that name in anyone's namespace.
fn conversation_head(config: &Config, conversation: &str) -> Result<Option<String>, String> {
    use conversation_protocol::v3::refs;
    let repo = config.repo.to_thread_local();
    let (namespace, id) = refs::parse_address(conversation)?;
    let refname = refs::head_ref(&namespace, &id)?;
    let Ok(mut reference) = repo.find_reference(refname.as_str()) else {
        return Ok(None);
    };
    let tree = reference
        .peel_to_commit()
        .map_err(|e| format!("{refname}: {e}"))?
        .tree_id()
        .map_err(|e| format!("{refname}: {e}"))?
        .to_string();
    Ok(Some(tree))
}

/// `path` in the conversation's head tree, evaluated the way the conversation
/// evaluates it: from the root of whichever tree along the path the walk
/// started at, a source tree's gitlink included.
fn match_conversation(
    config: &Config,
    head: &str,
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
    let mut tree = gix::ObjectId::from_hex(head.as_bytes()).map_err(|e| format!("{head}: {e}"))?;
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
