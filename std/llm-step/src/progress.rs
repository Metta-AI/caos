//! Append durable events to a conversation's one authoritative ref.
//!
//! The worker image has no `git`, so this module uses the server's object API
//! and exact-ref compare-and-append endpoint. Event objects are stored before
//! the ref moves. A successful return therefore means the event is reachable
//! from `refs/caos/v2/conversations/<id>/head`; failures are never downgraded
//! to observability warnings.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendResult {
    /// The event commit now reachable from the conversation ref.
    pub commit: String,
    /// The head this event commit directly follows on the first-parent spine.
    pub previous_head: String,
    /// Number of compare-and-swap races lost before the successful append.
    pub retries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConditionalAppend {
    Appended(AppendResult),
    /// Another event won before this event could be published. The contained
    /// hash is the head observed after the failed compare-and-swap.
    HeadChanged(String),
}

#[derive(Debug, Clone)]
struct RemoteCommit {
    tree: String,
    parents: Vec<String>,
    message: String,
}

/// One version-2 event on the canonical first-parent conversation spine.
#[derive(Debug, Clone)]
pub struct ConversationEvent {
    pub commit: String,
    pub tree: String,
    pub value: Value,
}

/// A chronological snapshot of the canonical conversation event log.
#[derive(Debug, Clone)]
pub struct ConversationLog {
    pub head: String,
    pub events: Vec<ConversationEvent>,
}

enum PushResult {
    Updated,
    Rejected(String),
}

/// Append only while `expected_head` is still the canonical tip. Unlike
/// a rebasing append, this never retries over a CAS loser. Terminal state uses
/// this guard so a user message that arrives at the end of a model round is
/// either answered by this request or starts a later request; it is never
/// buried underneath a blindly retried `status: idle` event.
pub fn append_event_at_head(
    conversation: &str,
    expected_head: &str,
    event: &Value,
    tree: &str,
) -> Result<ConditionalAppend, String> {
    let refname = conversation_ref(conversation)?;
    validate_hash(expected_head, "expected conversation head")?;
    validate_hash(tree, "event tree")?;
    validate_event(event)?;
    let base = server_base()?;
    let observed = read_ref(&base, &refname)?
        .ok_or_else(|| format!("conversation ref {refname} does not exist"))?;
    if observed != expected_head {
        return Ok(ConditionalAppend::HeadChanged(observed));
    }
    let current_tree = fetch_commit(&base, expected_head)?.tree;
    if current_tree != tree {
        return Err(format!(
            "conditional event would replace workspace tree {current_tree} with {tree}"
        ));
    }
    append_event_at_head_inner(&base, &refname, expected_head, event, tree, None)
}

/// Attempt one exact-head append, optionally retaining a workspace mutation as
/// a second parent. The caller is responsible for deriving `tree` from the
/// proposal's observed base before calling this function. A lost CAS is
/// returned, never silently rebased, so event-specific checks can be repeated.
pub fn append_event_at_head_with_parent(
    conversation: &str,
    expected_head: &str,
    event: &Value,
    tree: &str,
    extra_parent: Option<&str>,
) -> Result<ConditionalAppend, String> {
    let refname = conversation_ref(conversation)?;
    validate_hash(expected_head, "expected conversation head")?;
    validate_hash(tree, "event tree")?;
    if let Some(parent) = extra_parent {
        validate_hash(parent, "extra parent")?;
    }
    validate_event(event)?;
    let base = server_base()?;
    let observed = read_ref(&base, &refname)?
        .ok_or_else(|| format!("conversation ref {refname} does not exist"))?;
    if observed != expected_head {
        return Ok(ConditionalAppend::HeadChanged(observed));
    }
    append_event_at_head_inner(&base, &refname, expected_head, event, tree, extra_parent)
}

fn append_event_at_head_inner(
    base: &str,
    refname: &str,
    expected_head: &str,
    event: &Value,
    tree: &str,
    extra_parent: Option<&str>,
) -> Result<ConditionalAppend, String> {
    let message =
        serde_json::to_string(event).map_err(|e| format!("serializing conversation event: {e}"))?;
    let mut parents = vec![expected_head];
    if let Some(parent) = extra_parent {
        if parent != expected_head {
            parents.push(parent);
        }
    }
    let candidate = store_commit(base, tree, &parents, &message)?;
    let appended = || {
        ConditionalAppend::Appended(AppendResult {
            commit: candidate.clone(),
            previous_head: expected_head.to_string(),
            retries: 0,
        })
    };

    match push_ref(base, refname, expected_head, &candidate) {
        Ok(PushResult::Updated) => Ok(appended()),
        Ok(PushResult::Rejected(report)) => {
            let observed = read_ref(base, refname)?
                .ok_or_else(|| format!("conversation ref {refname} disappeared during append"))?;
            if observed == candidate || first_parent_contains(base, &observed, &candidate)? {
                Ok(appended())
            } else if observed != expected_head {
                Ok(ConditionalAppend::HeadChanged(observed))
            } else {
                Err(format!(
                    "server rejected update of {refname} without a CAS race: {report}"
                ))
            }
        }
        Err(error) => {
            let observed = read_ref(base, refname).map_err(|read_error| {
                format!(
                    "pushing {refname} failed ({error}); rereading it also failed: {read_error}"
                )
            })?;
            let Some(observed) = observed else {
                return Err(format!(
                    "conversation ref {refname} disappeared during append"
                ));
            };
            if observed == candidate || first_parent_contains(base, &observed, &candidate)? {
                Ok(appended())
            } else if observed != expected_head {
                Ok(ConditionalAppend::HeadChanged(observed))
            } else {
                Err(error)
            }
        }
    }
}

fn validate_event(event: &Value) -> Result<(), String> {
    if !event.is_object() || event.get("v").and_then(Value::as_u64) != Some(2) {
        return Err("conversation event must be a version-2 JSON object".to_string());
    }
    Ok(())
}

/// Read all version-2 events reachable from the canonical conversation head,
/// following only the first parent. The first non-event commit is the
/// workspace base and is deliberately excluded.
pub fn conversation_log(conversation: &str) -> Result<ConversationLog, String> {
    let refname = conversation_ref(conversation)?;
    let base = server_base()?;
    let head = read_ref(&base, &refname)?
        .ok_or_else(|| format!("conversation ref {refname} does not exist"))?;
    let mut newest_first = Vec::new();
    let mut current = head.clone();

    loop {
        let commit = fetch_commit(&base, &current)?;
        let value = match serde_json::from_str::<Value>(commit.message.trim()) {
            Ok(value) if value.is_object() && value.get("v").and_then(Value::as_u64) == Some(2) => {
                value
            }
            _ if !newest_first.is_empty() => break,
            _ => return Err(format!("conversation head {head} is not a version-2 event")),
        };
        let parent = commit.parents.first().cloned();
        newest_first.push(ConversationEvent {
            commit: current,
            tree: commit.tree,
            value,
        });
        let Some(parent) = parent else {
            break;
        };
        current = parent;
    }
    newest_first.reverse();
    Ok(ConversationLog {
        head,
        events: newest_first,
    })
}

fn conversation_ref(conversation: &str) -> Result<String, String> {
    validate_conversation(conversation)?;
    Ok(format!("refs/caos/v2/conversations/{conversation}/head"))
}

/// A conservative in-worker equivalent of `git check-ref-format` for the id
/// portion. In particular, `head` is reserved as *any* path component: allowing
/// `a/head/b` would make its ref collide with conversation `a`'s ref-as-file.
fn validate_conversation(conversation: &str) -> Result<(), String> {
    if conversation.is_empty() || conversation.len() > 512 {
        return Err(format!("invalid conversation name {conversation:?}"));
    }
    if conversation.starts_with('/')
        || conversation.ends_with('/')
        || conversation.contains("//")
        || conversation.contains("..")
        || conversation.contains("@{")
        || conversation.ends_with('.')
        || conversation.bytes().any(|b| {
            b <= b' ' || b == 0x7f || matches!(b, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
    {
        return Err(format!("invalid conversation name {conversation:?}"));
    }
    for component in conversation.split('/') {
        if component.is_empty()
            || component == "."
            || component == "head"
            || component.starts_with('.')
            || component.ends_with(".lock")
        {
            return Err(format!("invalid conversation name {conversation:?}"));
        }
    }
    Ok(())
}

fn validate_hash(hash: &str, what: &str) -> Result<(), String> {
    if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid {what} hash {hash:?}"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceConflict {
    pub paths: Vec<String>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryTree {
    Merged(String),
    Conflict(WorkspaceConflict),
}

/// Reconcile a tool's proposed workspace tree with changes appended to the
/// conversation after the tool started. Git itself is not present in the
/// llm-step image, so this performs the tree-level part of a three-way merge
/// directly against the server's object store. Changes to different paths
/// merge recursively; two changes to the same entry become a durable conflict
/// disposition so the caller can retain the proposal as an event parent.
pub fn retry_tree(base: &str, proposed: &str, current: &str) -> Result<RetryTree, String> {
    if current == base {
        return Ok(RetryTree::Merged(proposed.to_string()));
    } else if proposed == base || proposed == current {
        return Ok(RetryTree::Merged(current.to_string()));
    }

    validate_hash(base, "tool base tree")?;
    validate_hash(proposed, "tool proposed tree")?;
    validate_hash(current, "current conversation tree")?;
    let mut store = RemoteTreeStore {
        base: server_base()?,
    };
    match merge_tree(&mut store, Some(base), proposed, current, "") {
        Ok(tree) => Ok(RetryTree::Merged(tree)),
        Err(error) => match conflict_path(&error) {
            Some(path) => Ok(RetryTree::Conflict(WorkspaceConflict {
                paths: vec![path.to_string()],
                detail: error,
            })),
            None => Err(error),
        },
    }
}

const WORKSPACE_CONFLICT_PREFIX: &str = "conversation workspace conflict at ";

fn conflict_path(error: &str) -> Option<&str> {
    error
        .strip_prefix(WORKSPACE_CONFLICT_PREFIX)?
        .split_once(':')
        .map(|(path, _)| path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEntry {
    mode: u32,
    oid: String,
}

impl TreeEntry {
    fn is_tree(&self) -> bool {
        self.mode == 0o040000
    }

    fn is_regular_file(&self) -> bool {
        matches!(self.mode, 0o100644 | 0o100755)
    }
}

trait TreeStore {
    fn read_tree(&mut self, oid: &str) -> Result<BTreeMap<Vec<u8>, TreeEntry>, String>;
    fn write_tree(&mut self, entries: &BTreeMap<Vec<u8>, TreeEntry>) -> Result<String, String>;
}

struct RemoteTreeStore {
    base: String,
}

impl TreeStore for RemoteTreeStore {
    fn read_tree(&mut self, oid: &str) -> Result<BTreeMap<Vec<u8>, TreeEntry>, String> {
        validate_hash(oid, "tree")?;
        let serialized = fetch_object(&self.base, oid)?;
        parse_serialized_tree(&serialized).map_err(|error| format!("tree {oid}: {error}"))
    }

    fn write_tree(&mut self, entries: &BTreeMap<Vec<u8>, TreeEntry>) -> Result<String, String> {
        let content = serialize_tree(entries)?;
        store_object(&self.base, "tree", &content)
    }
}

/// Merge three trees with `base` as the common ancestor. `base = None` is the
/// empty tree used when both sides independently add the same directory.
fn merge_tree<S: TreeStore>(
    store: &mut S,
    base: Option<&str>,
    proposed: &str,
    current: &str,
    path: &str,
) -> Result<String, String> {
    if base == Some(current) {
        return Ok(proposed.to_string());
    }
    if base == Some(proposed) || proposed == current {
        return Ok(current.to_string());
    }

    let base_entries = match base {
        Some(oid) => store.read_tree(oid)?,
        None => BTreeMap::new(),
    };
    let proposed_entries = store.read_tree(proposed)?;
    let current_entries = store.read_tree(current)?;
    let mut names = BTreeSet::new();
    names.extend(base_entries.keys().cloned());
    names.extend(proposed_entries.keys().cloned());
    names.extend(current_entries.keys().cloned());

    let mut merged = BTreeMap::new();
    for name in names {
        let base_entry = base_entries.get(&name);
        let proposed_entry = proposed_entries.get(&name);
        let current_entry = current_entries.get(&name);
        let display_name = String::from_utf8_lossy(&name);
        let child_path = if path.is_empty() {
            display_name.into_owned()
        } else {
            format!("{path}/{display_name}")
        };
        if let Some(entry) = merge_entry(
            store,
            base_entry,
            proposed_entry,
            current_entry,
            &child_path,
        )? {
            merged.insert(name, entry);
        }
    }
    store.write_tree(&merged)
}

fn merge_entry<S: TreeStore>(
    store: &mut S,
    base: Option<&TreeEntry>,
    proposed: Option<&TreeEntry>,
    current: Option<&TreeEntry>,
    path: &str,
) -> Result<Option<TreeEntry>, String> {
    if current == base {
        return Ok(proposed.cloned());
    }
    if proposed == base || proposed == current {
        return Ok(current.cloned());
    }

    if let (Some(proposed), Some(current)) = (proposed, current) {
        if proposed.is_tree() && current.is_tree() && base.is_none_or(TreeEntry::is_tree) {
            let oid = merge_tree(
                store,
                base.map(|entry| entry.oid.as_str()),
                &proposed.oid,
                &current.oid,
                path,
            )?;
            return Ok(Some(TreeEntry {
                mode: 0o040000,
                oid,
            }));
        }
        if let Some(base) = base {
            // Git can cleanly combine a chmod on one side with a content edit
            // on the other. Restrict this component-wise merge to regular
            // files: a regular-file/symlink transition changes the meaning of
            // the blob and must remain a conflict.
            if base.is_regular_file() && proposed.is_regular_file() && current.is_regular_file() {
                if let (Some(mode), Some(oid)) = (
                    merge_scalar(base.mode, proposed.mode, current.mode),
                    merge_scalar(&base.oid, &proposed.oid, &current.oid),
                ) {
                    return Ok(Some(TreeEntry {
                        mode,
                        oid: oid.clone(),
                    }));
                }
            }
        }
    }

    Err(format!(
        "{WORKSPACE_CONFLICT_PREFIX}{path}: base {}, proposed {}, concurrent {}",
        describe_entry(base),
        describe_entry(proposed),
        describe_entry(current)
    ))
}

fn merge_scalar<T: Eq>(base: T, proposed: T, current: T) -> Option<T> {
    if current == base {
        Some(proposed)
    } else if proposed == base || proposed == current {
        Some(current)
    } else {
        None
    }
}

fn describe_entry(entry: Option<&TreeEntry>) -> String {
    match entry {
        Some(entry) => format!("{:o} {}", entry.mode, entry.oid),
        None => "absent".to_string(),
    }
}

fn parse_serialized_tree(serialized: &[u8]) -> Result<BTreeMap<Vec<u8>, TreeEntry>, String> {
    let content = serialized_object_content(serialized, "tree")?;
    let mut entries = BTreeMap::new();
    let mut offset = 0;
    while offset < content.len() {
        let mode_end = content[offset..]
            .iter()
            .position(|byte| *byte == b' ')
            .map(|position| offset + position)
            .ok_or("tree entry has no mode terminator")?;
        let mode_text = std::str::from_utf8(&content[offset..mode_end])
            .map_err(|error| format!("tree mode is not ASCII: {error}"))?;
        let mode = u32::from_str_radix(mode_text, 8)
            .map_err(|error| format!("invalid tree mode {mode_text:?}: {error}"))?;
        if !matches!(mode, 0o040000 | 0o100644 | 0o100755 | 0o120000 | 0o160000) {
            return Err(format!("unsupported tree mode {mode_text:?}"));
        }
        let name_start = mode_end + 1;
        let name_end = content[name_start..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|position| name_start + position)
            .ok_or("tree entry has no name terminator")?;
        let name = content[name_start..name_end].to_vec();
        if name.is_empty() || name.contains(&b'/') {
            return Err(format!("invalid tree entry name {name:?}"));
        }
        let oid_start = name_end + 1;
        let oid_end = oid_start + 20;
        if oid_end > content.len() {
            return Err("tree entry has a truncated object id".to_string());
        }
        let oid = hex_oid(&content[oid_start..oid_end]);
        if entries
            .insert(name.clone(), TreeEntry { mode, oid })
            .is_some()
        {
            return Err(format!("duplicate tree entry name {name:?}"));
        }
        offset = oid_end;
    }
    Ok(entries)
}

fn serialize_tree(entries: &BTreeMap<Vec<u8>, TreeEntry>) -> Result<Vec<u8>, String> {
    let mut ordered: Vec<_> = entries.iter().collect();
    ordered.sort_by(|(left_name, left), (right_name, right)| {
        git_tree_name_cmp(left_name, left.is_tree(), right_name, right.is_tree())
    });
    let mut content = Vec::new();
    for (name, entry) in ordered {
        if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
            return Err(format!("invalid tree entry name {name:?}"));
        }
        content.extend_from_slice(format!("{:o} ", entry.mode).as_bytes());
        content.extend_from_slice(name);
        content.push(0);
        content.extend_from_slice(&decode_oid(&entry.oid)?);
    }
    Ok(content)
}

/// Git compares a directory name as though it had a trailing slash. This is
/// observably different from sorting only by the raw name (`foo.bar` sorts
/// before directory `foo`).
fn git_tree_name_cmp(
    left: &[u8],
    left_is_tree: bool,
    right: &[u8],
    right_is_tree: bool,
) -> Ordering {
    let common = left.len().min(right.len());
    match left[..common].cmp(&right[..common]) {
        Ordering::Equal => {
            let left_next =
                left.get(common)
                    .copied()
                    .unwrap_or(if left_is_tree { b'/' } else { 0 });
            let right_next =
                right
                    .get(common)
                    .copied()
                    .unwrap_or(if right_is_tree { b'/' } else { 0 });
            left_next.cmp(&right_next)
        }
        ordering => ordering,
    }
}

fn decode_oid(oid: &str) -> Result<Vec<u8>, String> {
    validate_hash(oid, "tree entry")?;
    let mut bytes = Vec::with_capacity(20);
    for pair in oid.as_bytes().chunks_exact(2) {
        let pair = std::str::from_utf8(pair).expect("hex object id is ASCII");
        bytes.push(
            u8::from_str_radix(pair, 16)
                .map_err(|error| format!("invalid object id {oid:?}: {error}"))?,
        );
    }
    Ok(bytes)
}

fn hex_oid(bytes: &[u8]) -> String {
    let mut oid = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        oid.push_str(&format!("{byte:02x}"));
    }
    oid
}

fn store_commit(base: &str, tree: &str, parents: &[&str], message: &str) -> Result<String, String> {
    let content = commit_content(tree, parents, message)?;
    store_object(base, "commit", content.as_bytes())
}

fn commit_content(tree: &str, parents: &[&str], message: &str) -> Result<String, String> {
    validate_hash(tree, "commit tree")?;
    if parents.is_empty() {
        return Err("conversation event commit requires a parent".to_string());
    }
    let mut content = format!("tree {tree}\n");
    for parent in parents {
        validate_hash(parent, "commit parent")?;
        content.push_str(&format!("parent {parent}\n"));
    }
    // A fixed identity makes a proposal stable for a given event, tree, and
    // observed head. The event itself may carry a wall-clock timestamp when it
    // is useful to the transcript.
    content.push_str(
        "author caos-agent <caos@caos> 0 +0000\n\
         committer caos-agent <caos@caos> 0 +0000\n\n",
    );
    content.push_str(message);
    if !message.ends_with('\n') {
        content.push('\n');
    }
    Ok(content)
}

fn store_object(base: &str, kind: &str, content: &[u8]) -> Result<String, String> {
    let mut body = format!("{kind} {}\0", content.len()).into_bytes();
    body.extend_from_slice(content);
    let url = format!("{base}/object/");
    let resp = minreq::post(&url)
        .with_body(body)
        .with_timeout(30)
        .send()
        .map_err(|e| format!("POST {url}: {e}"))?;
    if !(200..300).contains(&resp.status_code) {
        return Err(format!(
            "POST {url}: {} {}",
            resp.status_code, resp.reason_phrase
        ));
    }
    let hash = resp
        .as_str()
        .map_err(|e| format!("POST {url}: invalid response: {e}"))?
        .trim()
        .to_string();
    validate_hash(&hash, "stored object")?;
    Ok(hash)
}

fn fetch_commit(base: &str, hash: &str) -> Result<RemoteCommit, String> {
    validate_hash(hash, "commit")?;
    let serialized = fetch_object(base, hash)?;
    parse_serialized_commit(&serialized)
        .map_err(|e| format!("object {hash}: invalid commit object: {e}"))
}

fn fetch_object(base: &str, hash: &str) -> Result<Vec<u8>, String> {
    validate_hash(hash, "object")?;
    let url = format!("{base}/object/{hash}");
    let resp = minreq::get(&url)
        .with_timeout(30)
        .send()
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !(200..300).contains(&resp.status_code) {
        return Err(format!(
            "GET {url}: {} {}",
            resp.status_code, resp.reason_phrase
        ));
    }
    Ok(resp.into_bytes())
}

/// Error-path recovery for an accepted push whose response was lost and whose
/// candidate already gained newer first-parent descendants before the reread.
fn first_parent_contains(base: &str, tip: &str, needle: &str) -> Result<bool, String> {
    let mut current = tip.to_string();
    for _ in 0..4096 {
        if current == needle {
            return Ok(true);
        }
        let commit = fetch_commit(base, &current)?;
        let Some(parent) = commit.parents.first() else {
            return Ok(false);
        };
        current = parent.clone();
    }
    Err(format!(
        "first-parent recovery walk from {tip} exceeded 4096 commits"
    ))
}

fn parse_serialized_commit(serialized: &[u8]) -> Result<RemoteCommit, String> {
    let content = serialized_object_content(serialized, "commit")?;
    let text =
        std::str::from_utf8(content).map_err(|e| format!("commit content is not UTF-8: {e}"))?;
    let (headers, message) = text
        .split_once("\n\n")
        .ok_or("commit has no header/message separator")?;
    let tree = headers
        .lines()
        .find_map(|line| line.strip_prefix("tree "))
        .ok_or("commit has no tree")?
        .to_string();
    validate_hash(&tree, "commit tree")?;
    let mut parents = Vec::new();
    for line in headers.lines() {
        if let Some(parent) = line.strip_prefix("parent ") {
            validate_hash(parent, "commit parent")?;
            parents.push(parent.to_string());
        }
    }
    Ok(RemoteCommit {
        tree,
        parents,
        message: message.to_string(),
    })
}

fn serialized_object_content<'a>(
    serialized: &'a [u8],
    expected_kind: &str,
) -> Result<&'a [u8], String> {
    let nul = serialized
        .iter()
        .position(|b| *b == 0)
        .ok_or("object has no header terminator")?;
    let header = std::str::from_utf8(&serialized[..nul])
        .map_err(|e| format!("object header is not UTF-8: {e}"))?;
    let (kind, size) = header
        .split_once(' ')
        .ok_or_else(|| format!("malformed object header {header:?}"))?;
    if kind != expected_kind {
        return Err(format!("expected {expected_kind}, got {kind:?}"));
    }
    let declared: usize = size
        .parse()
        .map_err(|e| format!("invalid object size {size:?}: {e}"))?;
    let content = &serialized[nul + 1..];
    if declared != content.len() {
        return Err(format!(
            "declared size {declared} does not match {} bytes",
            content.len()
        ));
    }
    Ok(content)
}

fn server_base() -> Result<String, String> {
    let base =
        std::env::var("CAOS_SERVER_URL").map_err(|_| "CAOS_SERVER_URL not set".to_string())?;
    Ok(base.trim_end_matches('/').to_string())
}

fn push_ref(
    base: &str,
    refname: &str,
    expected: &str,
    new_hash: &str,
) -> Result<PushResult, String> {
    validate_hash(expected, "expected ref")?;
    validate_hash(new_hash, "new ref")?;

    let body = serde_json::to_vec(&serde_json::json!({
        "ref": refname,
        "expected": expected,
        "new": new_hash,
    }))
    .map_err(|e| format!("serializing ref append: {e}"))?;
    let url = format!("{base}/ref/append");
    let resp = minreq::post(&url)
        .with_header("content-type", "application/json")
        .with_timeout(30)
        .with_body(body)
        .send()
        .map_err(|e| format!("POST {url}: {e}"))?;
    if (200..300).contains(&resp.status_code) {
        Ok(PushResult::Updated)
    } else if resp.status_code == 409 {
        Ok(PushResult::Rejected(
            String::from_utf8_lossy(resp.as_bytes()).trim().to_string(),
        ))
    } else {
        Err(format!(
            "POST {url}: {} {}: {}",
            resp.status_code,
            resp.reason_phrase,
            String::from_utf8_lossy(resp.as_bytes()).trim()
        ))
    }
}

/// Read exactly `refname`, without downloading the repository-wide Git ref
/// advertisement.
fn read_ref(base: &str, refname: &str) -> Result<Option<String>, String> {
    let body = serde_json::to_vec(&serde_json::json!({"ref": refname}))
        .map_err(|e| format!("serializing ref read: {e}"))?;
    let url = format!("{base}/ref/read");
    let resp = minreq::post(&url)
        .with_header("content-type", "application/json")
        .with_timeout(30)
        .with_body(body)
        .send()
        .map_err(|e| format!("POST {url}: {e}"))?;
    if resp.status_code == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&resp.status_code) {
        return Err(format!(
            "POST {url}: {} {}: {}",
            resp.status_code,
            resp.reason_phrase,
            String::from_utf8_lossy(resp.as_bytes()).trim()
        ));
    }
    let hash = resp
        .as_str()
        .map_err(|e| format!("POST {url}: {e}"))?
        .trim();
    validate_hash(hash, "remote ref")?;
    Ok(Some(hash.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    #[test]
    fn validates_conversation_ids_and_reserves_head_components() {
        assert_eq!(
            conversation_ref("project/talk-1").unwrap(),
            "refs/caos/v2/conversations/project/talk-1/head"
        );
        for invalid in [
            "", "bad name", "a//b", "a/../b", ".hidden", "a.lock", "head", "a/head/b", "a~b",
            "a@{b",
        ] {
            assert!(
                conversation_ref(invalid).is_err(),
                "accepted invalid id {invalid:?}"
            );
        }
    }

    #[derive(Default)]
    struct MemoryTreeStore {
        trees: BTreeMap<String, BTreeMap<Vec<u8>, TreeEntry>>,
        next: u64,
    }

    impl MemoryTreeStore {
        fn insert(&mut self, id: u64, entries: &[(&str, u32, u64)]) -> String {
            let oid = test_oid(id);
            self.trees.insert(
                oid.clone(),
                entries
                    .iter()
                    .map(|(name, mode, child)| {
                        (
                            name.as_bytes().to_vec(),
                            TreeEntry {
                                mode: *mode,
                                oid: test_oid(*child),
                            },
                        )
                    })
                    .collect(),
            );
            oid
        }
    }

    impl TreeStore for MemoryTreeStore {
        fn read_tree(&mut self, oid: &str) -> Result<BTreeMap<Vec<u8>, TreeEntry>, String> {
            self.trees
                .get(oid)
                .cloned()
                .ok_or_else(|| format!("test tree {oid} is absent"))
        }

        fn write_tree(&mut self, entries: &BTreeMap<Vec<u8>, TreeEntry>) -> Result<String, String> {
            self.next += 1;
            let oid = test_oid(10_000 + self.next);
            self.trees.insert(oid.clone(), entries.clone());
            Ok(oid)
        }
    }

    fn test_oid(id: u64) -> String {
        format!("{id:040x}")
    }

    fn tree_entry<'a>(entries: &'a BTreeMap<Vec<u8>, TreeEntry>, name: &str) -> &'a TreeEntry {
        entries.get(name.as_bytes()).unwrap()
    }

    #[test]
    fn retry_tree_keeps_tree_neutral_fast_paths() {
        assert_eq!(
            retry_tree(A, B, A).unwrap(),
            RetryTree::Merged(B.to_string())
        );
        assert_eq!(
            retry_tree(A, A, C).unwrap(),
            RetryTree::Merged(C.to_string())
        );
        assert_eq!(
            retry_tree(A, B, B).unwrap(),
            RetryTree::Merged(B.to_string())
        );
    }

    #[test]
    fn workspace_conflict_paths_are_structured() {
        let detail =
            format!("{WORKSPACE_CONFLICT_PREFIX}src/main.rs: base a, proposed b, concurrent c");
        assert_eq!(conflict_path(&detail), Some("src/main.rs"));
        assert_eq!(conflict_path("reading tree failed"), None);
    }

    #[test]
    fn three_way_tree_merge_combines_tool_and_async_status_changes() {
        let mut store = MemoryTreeStore::default();
        let source_base = store.insert(1, &[("main.rs", 0o100644, 101)]);
        let source_proposed = store.insert(2, &[("main.rs", 0o100644, 102)]);
        let async_base = store.insert(3, &[("status", 0o100644, 103)]);
        let async_current = store.insert(4, &[("status", 0o100644, 104)]);
        let base = store.insert(5, &[(".caos", 0o040000, 3), ("src", 0o040000, 1)]);
        let proposed = store.insert(6, &[(".caos", 0o040000, 3), ("src", 0o040000, 2)]);
        let current = store.insert(7, &[(".caos", 0o040000, 4), ("src", 0o040000, 1)]);

        let merged = merge_tree(&mut store, Some(&base), &proposed, &current, "").unwrap();
        let root = store.read_tree(&merged).unwrap();
        assert_eq!(tree_entry(&root, ".caos").oid, async_current);
        assert_eq!(tree_entry(&root, "src").oid, source_proposed);
        assert_ne!(source_base, source_proposed);
        assert_ne!(async_base, async_current);
    }

    #[test]
    fn three_way_tree_merge_recurses_for_changes_in_one_directory() {
        let mut store = MemoryTreeStore::default();
        store.insert(1, &[("left", 0o100644, 101), ("right", 0o100644, 102)]);
        store.insert(2, &[("left", 0o100644, 103), ("right", 0o100644, 102)]);
        store.insert(3, &[("left", 0o100644, 101), ("right", 0o100644, 104)]);
        let base = store.insert(4, &[("nested", 0o040000, 1)]);
        let proposed = store.insert(5, &[("nested", 0o040000, 2)]);
        let current = store.insert(6, &[("nested", 0o040000, 3)]);

        let merged = merge_tree(&mut store, Some(&base), &proposed, &current, "").unwrap();
        let root = store.read_tree(&merged).unwrap();
        let nested = store.read_tree(&tree_entry(&root, "nested").oid).unwrap();
        assert_eq!(tree_entry(&nested, "left").oid, test_oid(103));
        assert_eq!(tree_entry(&nested, "right").oid, test_oid(104));
    }

    #[test]
    fn three_way_tree_merge_combines_independently_added_directory_contents() {
        let mut store = MemoryTreeStore::default();
        let empty = store.insert(1, &[]);
        store.insert(2, &[("from-tool", 0o100644, 101)]);
        store.insert(3, &[("from-async", 0o100644, 102)]);
        let proposed = store.insert(4, &[("new", 0o040000, 2)]);
        let current = store.insert(5, &[("new", 0o040000, 3)]);

        let merged = merge_tree(&mut store, Some(&empty), &proposed, &current, "").unwrap();
        let root = store.read_tree(&merged).unwrap();
        let new_dir = store.read_tree(&tree_entry(&root, "new").oid).unwrap();
        assert_eq!(tree_entry(&new_dir, "from-tool").oid, test_oid(101));
        assert_eq!(tree_entry(&new_dir, "from-async").oid, test_oid(102));
    }

    #[test]
    fn three_way_tree_merge_reports_same_path_and_modify_delete_conflicts() {
        let mut store = MemoryTreeStore::default();
        let base = store.insert(1, &[("shared", 0o100644, 101)]);
        let proposed = store.insert(2, &[("shared", 0o100644, 102)]);
        let current = store.insert(3, &[("shared", 0o100644, 103)]);
        let error = merge_tree(&mut store, Some(&base), &proposed, &current, "").unwrap_err();
        assert!(error.contains("conflict at shared"), "{error}");

        let deleted = store.insert(4, &[]);
        let error = merge_tree(&mut store, Some(&base), &proposed, &deleted, "").unwrap_err();
        assert!(error.contains("conflict at shared"), "{error}");
        assert!(error.contains("concurrent absent"), "{error}");
    }

    #[test]
    fn three_way_tree_merge_combines_chmod_with_content_change() {
        let mut store = MemoryTreeStore::default();
        let base = store.insert(1, &[("script", 0o100644, 101)]);
        let proposed = store.insert(2, &[("script", 0o100755, 101)]);
        let current = store.insert(3, &[("script", 0o100644, 102)]);

        let merged = merge_tree(&mut store, Some(&base), &proposed, &current, "").unwrap();
        let root = store.read_tree(&merged).unwrap();
        assert_eq!(
            tree_entry(&root, "script"),
            &TreeEntry {
                mode: 0o100755,
                oid: test_oid(102),
            }
        );
    }

    #[test]
    fn tree_serialization_uses_git_order_and_round_trips() {
        let entries = BTreeMap::from([
            (
                b"foo".to_vec(),
                TreeEntry {
                    mode: 0o040000,
                    oid: test_oid(1),
                },
            ),
            (
                b"foo.bar".to_vec(),
                TreeEntry {
                    mode: 0o100644,
                    oid: test_oid(2),
                },
            ),
        ]);
        let content = serialize_tree(&entries).unwrap();
        assert!(content.starts_with(b"100644 foo.bar\0"));
        let mut serialized = format!("tree {}\0", content.len()).into_bytes();
        serialized.extend_from_slice(&content);
        assert_eq!(parse_serialized_tree(&serialized).unwrap(), entries);
    }

    #[test]
    fn only_version_two_objects_are_events() {
        assert!(validate_event(&serde_json::json!({"v": 2})).is_ok());
        assert!(validate_event(&serde_json::json!({"v": 1})).is_err());
        assert!(validate_event(&serde_json::json!({"status": "idle"})).is_err());
        assert!(validate_event(&serde_json::json!([])).is_err());
    }

    #[test]
    fn parses_git_serialized_commit() {
        let content = format!(
            "tree {A}\nparent {B}\nauthor x <x@x> 0 +0000\ncommitter x <x@x> 0 +0000\n\n{{\"author\":\"assistant\"}}\n"
        );
        let mut object = format!("commit {}\0", content.len()).into_bytes();
        object.extend_from_slice(content.as_bytes());
        let parsed = parse_serialized_commit(&object).unwrap();
        assert_eq!(parsed.tree, A);
    }

    #[test]
    fn event_commit_has_ordered_parents_and_stable_identity() {
        let content =
            commit_content(A, &[B, C], r#"{"author":"assistant","content":"hi"}"#).unwrap();
        assert_eq!(
            content,
            format!(
                "tree {A}\nparent {B}\nparent {C}\nauthor caos-agent <caos@caos> 0 +0000\ncommitter caos-agent <caos@caos> 0 +0000\n\n{{\"author\":\"assistant\",\"content\":\"hi\"}}\n"
            )
        );
    }

    #[test]
    fn invalid_object_size_is_rejected() {
        let object = format!("commit 1\0tree {A}\n\nmsg\n");
        assert!(parse_serialized_commit(object.as_bytes()).is_err());
    }
}
