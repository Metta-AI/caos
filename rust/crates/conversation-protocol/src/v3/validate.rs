use std::collections::{BTreeSet, HashSet};
use std::fmt;

use super::apply::{apply, Transition};
use super::canonical::parse_canonical;
use super::kinds::Kind;
use super::oid::{empty_tree, g3, Oid};
use super::paths::{self, WorkspaceFile};
use super::records::*;
use super::tree::{diff, Change, DryRunStore, Mode, ObjectStore};
use super::view::Conversation;

type Payloads = Vec<(String, Vec<u8>)>;
type FileChanges = Vec<(String, Option<(Mode, Vec<u8>)>)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Malformed {
    pub commit: Oid,
    pub reason: String,
}

impl fmt::Display for Malformed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "malformed conversation commit {}: {}",
            self.commit, self.reason
        )
    }
}

impl From<Malformed> for String {
    fn from(malformed: Malformed) -> String {
        malformed.to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Validated {
    pub parent: Oid,
    pub kind: Kind,
}

pub fn validate_commit(store: &dyn ObjectStore, commit: &Oid) -> Result<Validated, Malformed> {
    validate_commit_inner(store, commit).map_err(|reason| Malformed {
        commit: commit.clone(),
        reason,
    })
}

pub fn validate_spine(
    store: &dyn ObjectStore,
    head: &Oid,
    known_valid: &mut HashSet<Oid>,
) -> Result<Vec<Oid>, Malformed> {
    let mut current = head.clone();
    let mut validated = Vec::new();
    loop {
        if known_valid.contains(&current) {
            break;
        }
        if current == g3() {
            break;
        }
        let result = validate_commit(store, &current)?;
        validated.push(current.clone());
        if result.kind == Kind::ConversationRoot {
            break;
        }
        current = result.parent;
    }
    // A valid transition does not certify its ancestors. Publish the cache
    // entries only once the walk reaches a previously validated boundary.
    known_valid.extend(validated.iter().cloned());
    Ok(validated)
}

fn validate_commit_inner(store: &dyn ObjectStore, commit: &Oid) -> Result<Validated, String> {
    let info = store.read_commit(commit).map_err(String::from)?;
    if info.parents.len() != 1 {
        return Err("exactly one parent required".to_string());
    }
    let kind = Kind::parse_message(&info.message).map_err(|_| {
        format!(
            "commit message is not a registered kind: {:?}",
            String::from_utf8_lossy(&info.message)
        )
    })?;
    let parent = info.parents[0].clone();
    if kind == Kind::ConversationRoot && parent != g3() {
        return Err("root must parent G3".to_string());
    }
    if kind != Kind::ConversationRoot && parent == g3() {
        return Err("only a root may parent G3".to_string());
    }
    let parent_tree = if kind == Kind::ConversationRoot {
        empty_tree()
    } else {
        let parent_info = store.read_commit(&parent).map_err(String::from)?;
        Kind::parse_message(&parent_info.message)
            .map_err(|_| "parent is not a conversation commit".to_string())?;
        parent_info.tree
    };

    validate_root_shape(store, &info.tree)?;
    let child = Conversation::open_tree(store, &info.tree)?;
    let parent_view = if kind == Kind::ConversationRoot {
        None
    } else {
        Some(
            Conversation::open_tree(store, &parent_tree)
                .map_err(|error| format!("parent tree is malformed: {error}"))?,
        )
    };

    let changes = diff(
        store,
        (kind != Kind::ConversationRoot).then_some(&parent_tree),
        &info.tree,
    )?;
    if changes.is_empty() {
        return Err("no-op commit".to_string());
    }
    validate_delta(store, &changes)?;

    if kind == Kind::ConversationFork
        && child.identity()?.kind
            != (IdentityKind::Fork {
                source: parent.clone(),
            })
    {
        return Err("conversation fork identity does not name its source".to_string());
    }

    let transition = reconstruct(kind, parent_view.as_ref(), &child, &changes)?;
    let mut dry_run = DryRunStore::new(store);
    let applied = apply(
        &mut dry_run,
        (kind != Kind::ConversationRoot).then_some(&parent_tree),
        &transition,
    )
    .map_err(|error| format!("{}: {error}", kind.as_str()))?;
    if applied.tree != info.tree {
        return Err(first_differing_path(
            &dry_run,
            kind,
            &applied.tree,
            &info.tree,
        )?);
    }

    Ok(Validated { parent, kind })
}

fn validate_root_shape(store: &dyn ObjectStore, tree: &Oid) -> Result<(), String> {
    let entries = store.read_tree(tree).map_err(String::from)?;
    let caos_entries = entries
        .iter()
        .filter(|entry| entry.name == paths::CAOS_DIR)
        .count();
    let files_entries = entries
        .iter()
        .filter(|entry| entry.name == paths::FILES_DIR)
        .count();
    if caos_entries != 1 || files_entries > 1 {
        return Err("tree escapes .caos/ and files/".to_string());
    }
    for entry in &entries {
        let permitted = entry.name == paths::CAOS_DIR || entry.name == paths::FILES_DIR;
        if !permitted || entry.mode != Mode::Tree {
            return Err("tree escapes .caos/ and files/".to_string());
        }
    }
    let snapshot = super::tree::Snapshot::new(store, tree.clone());
    match snapshot.read(paths::FORMAT)? {
        Some(bytes) if bytes == paths::FORMAT_BYTES.as_bytes() => {}
        Some(_) => return Err("conversation format is invalid".to_string()),
        None => return Err("conversation format is missing".to_string()),
    }
    let identity = snapshot
        .read(paths::IDENTITY)?
        .ok_or_else(|| "identity is missing".to_string())?;
    Identity::parse(&identity).map_err(|error| format!("identity: {error}"))?;
    let title = snapshot
        .read(paths::TITLE)?
        .ok_or_else(|| "title is missing".to_string())?;
    parse_title(&title).map_err(|error| format!("title: {error}"))?;
    Ok(())
}

fn validate_delta(store: &dyn ObjectStore, changes: &[Change]) -> Result<(), String> {
    for change in changes {
        paths::validate_tree_path(&change.path)?;
        let under_caos = under(&change.path, paths::CAOS_DIR);
        let under_files = under(&change.path, paths::FILES_DIR);
        if !under_caos && !under_files {
            return Err("tree escapes .caos/ and files/".to_string());
        }
        if let Some((mode, oid)) = &change.after {
            if under_caos && *mode != Mode::Blob {
                return Err(format!("non-blob mode under .caos at {}", change.path));
            }
            if under_files && !matches!(mode, Mode::Blob | Mode::Executable) {
                return Err(format!("unsupported mode under files at {}", change.path));
            }
            if canonical_record_path(&change.path) {
                let bytes = store.read_blob(oid).map_err(String::from)?;
                parse_canonical(&bytes)
                    .map_err(|error| format!("unparseable record at {}: {error}", change.path))?;
            }
        }
    }
    Ok(())
}

fn canonical_record_path(path: &str) -> bool {
    path == paths::IDENTITY
        || matches!(
            paths::parse_workspace_path(path),
            Some((_, WorkspaceFile::Origin))
        )
        || transcript_record_path(path)
        || request_record_id(path).is_some()
        || is_tool_record_path(path)
        || async_record_id(path).is_some()
        || child_record_id(path).is_some()
        || publication_record_id(path).is_some()
}

fn reconstruct(
    kind: Kind,
    parent_snapshot: Option<&Conversation<'_>>,
    child_snapshot: &Conversation<'_>,
    changes: &[Change],
) -> Result<Transition, String> {
    if kind != Kind::ConversationRoot && parent_snapshot.is_none() {
        return Err(format!("{}: parent snapshot is missing", kind.as_str()));
    }
    match kind {
        Kind::ConversationRoot => {
            let workspaces = child_snapshot
                .workspaces()?
                .into_iter()
                .map(|(name, record)| (name, (record.commit, record.origin)))
                .collect();
            let files_seed = child_snapshot
                .snapshot()
                .entry(paths::FILES_DIR)?
                .map(|entry| entry.oid);
            Ok(Transition::ConversationRoot {
                identity: child_snapshot.identity()?,
                title: child_snapshot.title()?,
                workspaces,
                files_seed,
            })
        }
        Kind::ConversationFork => Ok(Transition::ConversationFork {
            identity: child_snapshot.identity()?,
            title: child_snapshot.title()?,
        }),
        Kind::MetadataTitleSet => Ok(Transition::TitleSet {
            title: child_snapshot.title()?,
        }),
        Kind::MessageAppend => {
            let (entry, payloads) = transcript_change(kind, child_snapshot, changes)?;
            Ok(Transition::MessageAppend { entry, payloads })
        }
        Kind::RequestAdmit => Ok(Transition::RequestAdmit {
            record: request_change(kind, child_snapshot, changes)?.1,
        }),
        Kind::RequestClaim => {
            let (request, record) = request_change(kind, child_snapshot, changes)?;
            let latest_message = record.latest_message.ok_or_else(|| {
                format!("{}: changed request has no latest_message", kind.as_str())
            })?;
            Ok(Transition::RequestClaim {
                request,
                latest_message,
            })
        }
        Kind::RequestInterject => {
            let (request, _) = request_change(kind, child_snapshot, changes)?;
            let (entry, payloads) = transcript_change(kind, child_snapshot, changes)?;
            Ok(Transition::RequestInterject {
                request,
                entry,
                payloads,
            })
        }
        Kind::RequestEscape => {
            let (request, record) = request_change(kind, child_snapshot, changes)?;
            Ok(Transition::RequestEscape {
                request,
                reason: record.escape_reason,
            })
        }
        Kind::RequestTerminal => {
            let (request, record) = request_change(kind, child_snapshot, changes)?;
            let outcome = record
                .outcome
                .ok_or_else(|| format!("{}: changed request has no outcome", kind.as_str()))?;
            Ok(Transition::RequestTerminal { request, outcome })
        }
        Kind::ModelComplete => {
            let (_, record) = request_change(kind, child_snapshot, changes)?;
            let (entry, payloads) = transcript_change(kind, child_snapshot, changes)?;
            let request = entry
                .request
                .clone()
                .ok_or_else(|| format!("{}: transcript entry has no request", kind.as_str()))?;
            Ok(Transition::ModelComplete {
                request,
                entry,
                payloads,
                calls: record.calls,
            })
        }
        Kind::ToolStart => Ok(Transition::ToolStart {
            record: tool_change(kind, child_snapshot, changes)?.1,
        }),
        Kind::ToolComplete => {
            let (_, record) = tool_change(kind, child_snapshot, changes)?;
            let payload_dir =
                paths::tool_payload_dir(record.request.as_str(), record.round, &record.id);
            Ok(Transition::ToolComplete {
                payloads: payload_changes(kind, child_snapshot, changes, &payload_dir)?,
                files: file_changes(child_snapshot, changes)?,
                record,
            })
        }
        Kind::AsyncStart => Ok(Transition::AsyncStart {
            record: async_change(kind, child_snapshot, changes)?.1,
        }),
        Kind::AsyncTerminal => {
            let (task, record) = async_change(kind, child_snapshot, changes)?;
            Ok(Transition::AsyncTerminal {
                task,
                status: record.status,
                result: record.result,
                reason: record.reason,
            })
        }
        Kind::SubagentSpawn => {
            let (_, tool) = tool_change(kind, child_snapshot, changes)?;
            let (_, child) = child_change(kind, child_snapshot, changes)?;
            let payload_dir = paths::tool_payload_dir(tool.request.as_str(), tool.round, &tool.id);
            Ok(Transition::SubagentSpawn {
                payloads: payload_changes(kind, child_snapshot, changes, &payload_dir)?,
                tool,
                child,
            })
        }
        Kind::SubagentTerminal => {
            let (child, record) = child_change(kind, child_snapshot, changes)?;
            let terminal_head = record
                .terminal_head
                .ok_or_else(|| format!("{}: changed child has no terminal_head", kind.as_str()))?;
            let child_workspaces = record.child_workspaces.ok_or_else(|| {
                format!("{}: changed child has no child_workspaces", kind.as_str())
            })?;
            Ok(Transition::SubagentTerminal {
                child,
                terminal_head,
                status: record.status,
                child_workspaces,
            })
        }
        Kind::SubagentApply => {
            let (child, record) = child_change(kind, child_snapshot, changes)?;
            let application =
                record.applications.last().cloned().ok_or_else(|| {
                    format!("{}: changed child has no application", kind.as_str())
                })?;
            Ok(Transition::SubagentApply { child, application })
        }
        Kind::WorkspaceCreate => {
            let name = single_workspace_name(changes, kind)?;
            let record = child_snapshot
                .workspace(&name)?
                .ok_or_else(|| format!("{}: changed workspace is absent", kind.as_str()))?;
            Ok(Transition::WorkspaceCreate {
                name,
                commit: record.commit,
                origin: record.origin,
            })
        }
        Kind::WorkspaceRollback => {
            let name = single_workspace_name(changes, kind)?;
            let record = child_snapshot
                .workspace(&name)?
                .ok_or_else(|| format!("{}: changed workspace is absent", kind.as_str()))?;
            Ok(Transition::WorkspaceRollback {
                name,
                commit: record.commit,
            })
        }
        Kind::WorkspaceRemove => Ok(Transition::WorkspaceRemove {
            name: single_workspace_name(changes, kind)?,
        }),
        Kind::PublicationPending => Ok(Transition::PublicationPending {
            record: publication_change(kind, child_snapshot, changes)?.1,
        }),
        Kind::PublicationTerminal => {
            let (publication, record) = publication_change(kind, child_snapshot, changes)?;
            let evidence = record
                .evidence
                .ok_or_else(|| format!("{}: changed publication has no evidence", kind.as_str()))?;
            Ok(Transition::PublicationTerminal {
                publication,
                status: record.status,
                evidence,
                observed: record.observed,
            })
        }
        Kind::FilesApply => Ok(Transition::FilesApply {
            files: file_changes(child_snapshot, changes)?,
        }),
    }
}

fn transcript_change(
    kind: Kind,
    child: &Conversation<'_>,
    changes: &[Change],
) -> Result<(TranscriptEntry, Payloads), String> {
    let path = single_change(changes, kind, "transcript record", transcript_record_path)?;
    let bytes = child
        .snapshot()
        .read(path)?
        .ok_or_else(|| format!("{}: changed transcript record is absent", kind.as_str()))?;
    let entry = TranscriptEntry::parse(&bytes)
        .map_err(|error| format!("{}: transcript record is invalid: {error}", kind.as_str()))?;
    let (ordinal, message_id) = paths::parse_transcript_entry_path(path)
        .map_err(|error| format!("{}: transcript path is invalid: {error}", kind.as_str()))?;
    let payload_dir = paths::transcript_payload_dir(ordinal, &message_id);
    let payloads = payload_changes(kind, child, changes, &payload_dir)?;
    Ok((entry, payloads))
}

fn request_change(
    kind: Kind,
    child: &Conversation<'_>,
    changes: &[Change],
) -> Result<(Oid, RequestRecord), String> {
    let path = single_change(changes, kind, "request record", |path| {
        request_record_id(path).is_some()
    })?;
    let request = request_record_id(path)
        .ok_or_else(|| format!("{}: request record path is invalid", kind.as_str()))?;
    let record = child
        .request(&request)?
        .ok_or_else(|| format!("{}: changed request record is absent", kind.as_str()))?;
    Ok((request, record))
}

fn tool_change(
    kind: Kind,
    child: &Conversation<'_>,
    changes: &[Change],
) -> Result<(String, ToolRecord), String> {
    let path = single_change(changes, kind, "tool record", is_tool_record_path)?;
    let bytes = child
        .snapshot()
        .read(path)?
        .ok_or_else(|| format!("{}: changed tool record is absent", kind.as_str()))?;
    let record = ToolRecord::parse(&bytes)
        .map_err(|error| format!("{}: tool record is invalid: {error}", kind.as_str()))?;
    if paths::tool_record_path(record.request.as_str(), record.round, &record.id) != path {
        return Err(format!(
            "{}: tool record identity does not match its path",
            kind.as_str()
        ));
    }
    Ok((path.to_string(), record))
}

fn async_change(
    kind: Kind,
    child: &Conversation<'_>,
    changes: &[Change],
) -> Result<(Oid, AsyncRecord), String> {
    let path = single_change(changes, kind, "async record", |path| {
        async_record_id(path).is_some()
    })?;
    let task = async_record_id(path)
        .ok_or_else(|| format!("{}: async record path is invalid", kind.as_str()))?;
    let record = child
        .async_task(&task)?
        .ok_or_else(|| format!("{}: changed async record is absent", kind.as_str()))?;
    Ok((task, record))
}

fn child_change(
    kind: Kind,
    child: &Conversation<'_>,
    changes: &[Change],
) -> Result<(String, ChildRecord), String> {
    let path = single_change(changes, kind, "child record", |path| {
        child_record_id(path).is_some()
    })?;
    let id = child_record_id(path)
        .ok_or_else(|| format!("{}: child record path is invalid", kind.as_str()))?;
    let record = child
        .child(id)?
        .ok_or_else(|| format!("{}: changed child record is absent", kind.as_str()))?;
    Ok((id.to_string(), record))
}

fn publication_change(
    kind: Kind,
    child: &Conversation<'_>,
    changes: &[Change],
) -> Result<(String, PublicationRecord), String> {
    let path = single_change(changes, kind, "publication record", |path| {
        publication_record_id(path).is_some()
    })?;
    let id = publication_record_id(path)
        .ok_or_else(|| format!("{}: publication record path is invalid", kind.as_str()))?;
    let record = child
        .publication(id)?
        .ok_or_else(|| format!("{}: changed publication record is absent", kind.as_str()))?;
    Ok((id.to_string(), record))
}

fn single_change<'a>(
    changes: &'a [Change],
    kind: Kind,
    what: &str,
    matches: impl Fn(&str) -> bool,
) -> Result<&'a str, String> {
    let mut paths = changes
        .iter()
        .filter(|change| matches(&change.path))
        .map(|change| change.path.as_str());
    let Some(path) = paths.next() else {
        return Err(format!("{}: no single {what} changed", kind.as_str()));
    };
    if paths.next().is_some() {
        return Err(format!("{}: no single {what} changed", kind.as_str()));
    }
    Ok(path)
}

fn single_workspace_name(changes: &[Change], kind: Kind) -> Result<String, String> {
    let names: BTreeSet<String> = changes
        .iter()
        .filter_map(|change| paths::parse_workspace_path(&change.path).map(|(name, _)| name))
        .collect();
    if names.len() != 1 {
        return Err(format!("{}: no single workspace changed", kind.as_str()));
    }
    Ok(names.into_iter().next().expect("one workspace name"))
}

fn payload_changes(
    kind: Kind,
    child: &Conversation<'_>,
    changes: &[Change],
    directory: &str,
) -> Result<Payloads, String> {
    let prefix = format!("{directory}/");
    changes
        .iter()
        .filter(|change| change.path.starts_with(&prefix))
        .map(|change| {
            let name = change
                .path
                .strip_prefix(&prefix)
                .expect("payload prefix was checked")
                .to_string();
            let bytes = child.snapshot().read(&change.path)?.ok_or_else(|| {
                format!(
                    "{}: changed payload {:?} is absent",
                    kind.as_str(),
                    change.path
                )
            })?;
            Ok((name, bytes))
        })
        .collect()
}

fn file_changes(child: &Conversation<'_>, changes: &[Change]) -> Result<FileChanges, String> {
    changes
        .iter()
        .filter_map(|change| {
            change
                .path
                .strip_prefix(&format!("{}/", paths::FILES_DIR))
                .map(|relative| (relative.to_string(), change))
        })
        .map(|(relative, change)| {
            let value = match &change.after {
                Some((mode, _)) => Some((
                    *mode,
                    child
                        .snapshot()
                        .read(&change.path)?
                        .ok_or_else(|| format!("changed file {:?} is absent", change.path))?,
                )),
                None => None,
            };
            Ok((relative, value))
        })
        .collect()
}

fn first_differing_path(
    store: &dyn ObjectStore,
    kind: Kind,
    reapplied: &Oid,
    child: &Oid,
) -> Result<String, String> {
    let changes = diff(store, Some(reapplied), child)?;
    let Some(first) = changes.first() else {
        return Ok(format!(
            "{}: re-applied tree oid differs without a leaf delta",
            kind.as_str()
        ));
    };
    let difference = match (&first.before, &first.after) {
        (Some(_), None) => "re-application kept it but child deleted it",
        (None, Some(_)) => "re-application deleted it but child kept it",
        (Some(_), Some(_)) => "re-application and child have different values",
        (None, None) => "re-application and child differ",
    };
    Ok(format!(
        "{}: re-applied tree differs first at path {} ({difference})",
        kind.as_str(),
        first.path
    ))
}

fn under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn transcript_record_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix(&format!("{}/", paths::TRANSCRIPT_DIR)) else {
        return false;
    };
    rest.matches('/').count() == 1 && paths::parse_transcript_entry_path(path).is_ok()
}

fn request_record_id(path: &str) -> Option<Oid> {
    oid_record_id(path, paths::REQUESTS_DIR)
}

fn is_tool_record_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix(&format!("{}/", paths::TOOLS_DIR)) else {
        return false;
    };
    let components: Vec<&str> = rest.split('/').collect();
    components.len() == 3 && components[2].ends_with(".json")
}

fn async_record_id(path: &str) -> Option<Oid> {
    oid_record_id(path, paths::ASYNC_DIR)
}

fn child_record_id(path: &str) -> Option<&str> {
    component_record_id(path, paths::SUBAGENTS_DIR)
}

fn publication_record_id(path: &str) -> Option<&str> {
    component_record_id(path, paths::PUBLICATIONS_DIR)
}

fn oid_record_id(path: &str, dir: &str) -> Option<Oid> {
    let id = component_record_id(path, dir)?;
    Oid::parse(id, "record id").ok()
}

fn component_record_id<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(&format!("{dir}/"))?;
    if rest.contains('/') {
        return None;
    }
    let id = rest.strip_suffix(".json")?;
    paths::validate_protocol_id_component(id).ok()?;
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v3::apply::client_signature;
    use crate::v3::canonical::canonical_bytes;
    use crate::v3::fixtures::golden;
    use crate::v3::tree::{CommitInfo, MemoryStore, ObjectStore, TreeBuilder};

    fn setup() -> (MemoryStore, Oid, Oid) {
        let mut store = MemoryStore::new();
        let fork = golden(&mut store);
        let source = store.read_commit(&fork).unwrap().parents[0].clone();
        (store, fork, source)
    }

    fn write_commit(
        store: &mut MemoryStore,
        tree: Oid,
        parents: Vec<Oid>,
        message: Vec<u8>,
    ) -> Oid {
        let signature = client_signature("Bad", "bad@example.com", 1_700_000_001);
        store
            .write_commit(&CommitInfo {
                tree,
                parents,
                author: signature.clone(),
                committer: signature,
                message,
            })
            .unwrap()
    }

    fn mutate(
        store: &mut MemoryStore,
        parent: &Oid,
        kind: Kind,
        edit: impl FnOnce(&mut TreeBuilder),
    ) -> Oid {
        let tree = store.read_commit(parent).unwrap().tree;
        let mut builder = TreeBuilder::from(Some(tree));
        edit(&mut builder);
        let tree = builder.build(store).unwrap();
        write_commit(store, tree, vec![parent.clone()], kind.message())
    }

    fn assert_reason(store: &MemoryStore, commit: &Oid, expected: &str) {
        let error = validate_commit(store, commit).unwrap_err();
        assert!(
            error.reason.contains(expected),
            "expected {expected:?} in {:?}",
            error.reason
        );
    }

    fn find_kind(store: &MemoryStore, head: &Oid, kind: Kind) -> Oid {
        find_commit(store, head, |view| view.kind() == Some(kind))
    }

    fn find_commit(
        store: &MemoryStore,
        head: &Oid,
        predicate: impl Fn(&Conversation<'_>) -> bool,
    ) -> Oid {
        let mut current = head.clone();
        loop {
            let view = Conversation::open(store, &current).unwrap();
            if predicate(&view) {
                return current;
            }
            current = view.parent().unwrap().clone();
            assert_ne!(current, g3());
        }
    }

    fn clone_delta(store: &mut MemoryStore, valid: &Oid) -> (Oid, TreeBuilder) {
        let info = store.read_commit(valid).unwrap();
        let parent = info.parents[0].clone();
        let parent_tree = store.read_commit(&parent).unwrap().tree;
        let changes = diff(store, Some(&parent_tree), &info.tree).unwrap();
        let mut builder = TreeBuilder::from(Some(parent_tree));
        for change in changes {
            match change.after {
                Some((mode, oid)) => builder.put_oid(&change.path, mode, oid),
                None => builder.delete(&change.path),
            }
        }
        (parent, builder)
    }

    fn fork_without_apply(store: &mut MemoryStore, source: &Oid, id: &str) -> Oid {
        let identity = Identity {
            id: id.to_string(),
            kind: IdentityKind::Fork {
                source: source.clone(),
            },
            owner: None,
        };
        mutate(store, source, Kind::ConversationFork, |builder| {
            builder.put(paths::IDENTITY, Mode::Blob, identity.encode());
        })
    }

    fn source_with_record(
        store: &mut MemoryStore,
        source: &Oid,
        path: &str,
        bytes: Vec<u8>,
    ) -> Oid {
        mutate(store, source, Kind::FilesApply, |builder| {
            builder.put(path, Mode::Blob, bytes);
        })
    }

    fn request_record(id: Oid, head: Oid) -> RequestRecord {
        RequestRecord {
            id,
            request_head: head,
            request_workspaces: None,
            model: "model".to_string(),
            configuration: "configuration-hash".to_string(),
            round: 0,
            calls: Vec::new(),
            interjections: Vec::new(),
            status: RequestStatus::Queued,
            latest_message: None,
            escape_reason: None,
            outcome: None,
        }
    }

    fn oid(character: char) -> Oid {
        Oid::parse(&character.to_string().repeat(40), "test oid").unwrap()
    }

    #[test]
    fn golden_spine_validates() {
        let mut store = MemoryStore::new();
        let head = golden(&mut store);
        let mut known_valid = HashSet::new();
        let validated = validate_spine(&store, &head, &mut known_valid).unwrap();
        assert_eq!(validated.len(), 32);
        assert_eq!(validated[0], head);
        let kinds: HashSet<Kind> = validated
            .iter()
            .map(|commit| Kind::parse_message(&store.read_commit(commit).unwrap().message).unwrap())
            .collect();
        assert_eq!(kinds, Kind::ALL.into_iter().collect());
        assert_eq!(
            validate_spine(&store, &head, &mut known_valid),
            Ok(Vec::new())
        );
    }

    #[test]
    fn parentless_commit_is_malformed() {
        let (mut store, _, source) = setup();
        let tree = store.read_commit(&source).unwrap().tree;
        let bad = write_commit(&mut store, tree, Vec::new(), Kind::FilesApply.message());
        assert_reason(&store, &bad, "exactly one parent");
    }

    #[test]
    fn two_parent_commit_is_malformed() {
        let (mut store, _, source) = setup();
        let tree = store.read_commit(&source).unwrap().tree;
        let bad = write_commit(
            &mut store,
            tree,
            vec![source.clone(), source],
            Kind::FilesApply.message(),
        );
        assert_reason(&store, &bad, "exactly one parent");
    }

    #[test]
    fn root_must_parent_g3() {
        let (mut store, _, source) = setup();
        let tree = store.read_commit(&source).unwrap().tree;
        let bad = write_commit(
            &mut store,
            tree,
            vec![source],
            Kind::ConversationRoot.message(),
        );
        assert_reason(&store, &bad, "root must parent G3");
    }

    #[test]
    fn only_root_may_parent_g3() {
        let (mut store, _, source) = setup();
        let tree = store.read_commit(&source).unwrap().tree;
        let bad = write_commit(
            &mut store,
            tree,
            vec![g3()],
            Kind::MetadataTitleSet.message(),
        );
        assert_reason(&store, &bad, "only a root may parent G3");
    }

    #[test]
    fn json_kind_message_is_malformed() {
        let (mut store, _, source) = setup();
        let tree = store.read_commit(&source).unwrap().tree;
        let bad = write_commit(
            &mut store,
            tree,
            vec![source],
            b"{\"kind\":\"tool.complete\"}\n".to_vec(),
        );
        assert_reason(&store, &bad, "not a registered kind");
    }

    #[test]
    fn kind_message_without_lf_is_malformed() {
        let (mut store, _, source) = setup();
        let tree = store.read_commit(&source).unwrap().tree;
        let bad = write_commit(&mut store, tree, vec![source], b"tool.complete".to_vec());
        assert_reason(&store, &bad, "not a registered kind");
    }

    #[test]
    fn blob_at_tree_root_is_malformed() {
        let (mut store, _, source) = setup();
        let bad = mutate(&mut store, &source, Kind::FilesApply, |builder| {
            builder.put("escape", Mode::Blob, b"bad".to_vec());
        });
        assert_reason(&store, &bad, "tree escapes");
    }

    #[test]
    fn wrong_format_is_malformed() {
        let (mut store, _, source) = setup();
        let bad = mutate(&mut store, &source, Kind::FilesApply, |builder| {
            builder.put(paths::FORMAT, Mode::Blob, b"wrong\n".to_vec());
        });
        assert_reason(&store, &bad, "format");
    }

    #[test]
    fn missing_title_is_malformed() {
        let (mut store, _, source) = setup();
        let bad = mutate(&mut store, &source, Kind::FilesApply, |builder| {
            builder.delete(paths::TITLE);
        });
        assert_reason(&store, &bad, "title");
    }

    #[test]
    fn title_set_cannot_change_files() {
        let (mut store, _, source) = setup();
        let bad = mutate(&mut store, &source, Kind::MetadataTitleSet, |builder| {
            builder.put(paths::TITLE, Mode::Blob, encode_title("Bad title").unwrap());
            builder.put("files/x", Mode::Blob, b"x".to_vec());
        });
        assert_reason(
            &store,
            &bad,
            "metadata.title.set: re-applied tree differs first at path files/x",
        );
    }

    #[test]
    fn no_op_commit_is_malformed() {
        let (mut store, _, source) = setup();
        let tree = store.read_commit(&source).unwrap().tree;
        let bad = write_commit(
            &mut store,
            tree,
            vec![source],
            Kind::MetadataTitleSet.message(),
        );
        assert_reason(&store, &bad, "no-op commit");
    }

    #[test]
    fn unsorted_record_keys_are_malformed() {
        let (mut store, _, source) = setup();
        let bad = mutate(&mut store, &source, Kind::MetadataTitleSet, |builder| {
            builder.put(
                &paths::request_record_path(oid('1').as_str()),
                Mode::Blob,
                b"{\"status\":\"idle\",\"id\":\"1111111111111111111111111111111111111111\"}\n"
                    .to_vec(),
            );
        });
        assert_reason(&store, &bad, "unparseable record");
    }

    #[test]
    fn unknown_request_key_is_malformed() {
        let (mut store, fork, _) = setup();
        let parent = find_commit(&store, &fork, |view| {
            view.kind() == Some(Kind::RequestClaim)
                && view
                    .active_request()
                    .unwrap()
                    .is_some_and(|request| request.status == RequestStatus::Running)
        });
        let view = Conversation::open(&store, &parent).unwrap();
        let record = view.active_request().unwrap().unwrap();
        let mut value = record.to_value();
        value
            .as_object_mut()
            .unwrap()
            .insert("unknown".to_string(), serde_json::Value::Null);
        let bytes = canonical_bytes(&value).unwrap();
        let path = paths::request_record_path(record.id.as_str());
        let bad = mutate(&mut store, &parent, Kind::RequestClaim, |builder| {
            builder.put(&path, Mode::Blob, bytes);
        });
        assert_reason(&store, &bad, "unknown field `unknown`");
    }

    #[test]
    fn transcript_modification_is_malformed() {
        let (mut store, _, source) = setup();
        let path = paths::transcript_entry_path(0, "user-0");
        let mut entry = TranscriptEntry::parse(
            &Conversation::open(&store, &source)
                .unwrap()
                .snapshot()
                .read(&path)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        entry.actor = "changed".to_string();
        let bad = mutate(&mut store, &source, Kind::MessageAppend, |builder| {
            builder.put(&path, Mode::Blob, entry.encode());
        });
        assert_reason(&store, &bad, "000000000000-user-0/body");
    }

    #[test]
    fn transcript_deletion_is_malformed() {
        let (mut store, _, source) = setup();
        let path = paths::transcript_entry_path(0, "user-0");
        let bad = mutate(&mut store, &source, Kind::MessageAppend, |builder| {
            builder.delete(&path);
        });
        assert_reason(&store, &bad, "changed transcript record is absent");
    }

    #[test]
    fn transcript_ordinal_must_be_dense() {
        let (mut store, _, source) = setup();
        let view = Conversation::open(&store, &source).unwrap();
        let ordinal = view.transcript_len().unwrap() + 1;
        let entry = transcript_entry(ordinal, "gap", None);
        let path = paths::transcript_entry_path(ordinal, "gap");
        let bad = mutate(&mut store, &source, Kind::MessageAppend, |builder| {
            builder.put(&path, Mode::Blob, entry.encode());
        });
        assert_reason(&store, &bad, "000000000006-gap.json");
    }

    #[test]
    fn transcript_message_id_must_match_path() {
        let (mut store, _, source) = setup();
        let view = Conversation::open(&store, &source).unwrap();
        let ordinal = view.transcript_len().unwrap();
        let entry = transcript_entry(ordinal, "record-id", None);
        let path = paths::transcript_entry_path(ordinal, "path-id");
        let bad = mutate(&mut store, &source, Kind::MessageAppend, |builder| {
            builder.put(&path, Mode::Blob, entry.encode());
        });
        assert_reason(&store, &bad, "000000000006-path-id.json");
    }

    #[test]
    fn transcript_payload_reference_must_be_added() {
        let (mut store, _, source) = setup();
        let view = Conversation::open(&store, &source).unwrap();
        let ordinal = view.transcript_len().unwrap();
        let missing = format!(
            "{}/missing",
            paths::transcript_payload_dir(ordinal, "missing-payload")
        );
        let entry = transcript_entry(ordinal, "missing-payload", Some(missing));
        let path = paths::transcript_entry_path(ordinal, "missing-payload");
        let bad = mutate(&mut store, &source, Kind::MessageAppend, |builder| {
            builder.put(&path, Mode::Blob, entry.encode());
        });
        assert_reason(&store, &bad, "missing-payload/missing");
    }

    #[test]
    fn title_set_cannot_move_workspace_pointer() {
        let (mut store, _, source) = setup();
        let bad = mutate(&mut store, &source, Kind::MetadataTitleSet, |builder| {
            builder.put(paths::TITLE, Mode::Blob, encode_title("Moved").unwrap());
            builder.put(
                &paths::workspace_commit_path("main"),
                Mode::Blob,
                oid('e').encode_line(),
            );
        });
        assert_reason(
            &store,
            &bad,
            "metadata.title.set: re-applied tree differs first at path .caos/workspaces/main/commit",
        );
    }

    #[test]
    fn interjection_pointer_must_match_resolution() {
        let (mut store, fork, _) = setup();
        let valid = find_commit(&store, &fork, |view| {
            view.kind() == Some(Kind::RequestInterject)
                && view.workspace("main").unwrap().unwrap().commit == oid('d')
        });
        let (parent, mut builder) = clone_delta(&mut store, &valid);
        builder.put(
            &paths::workspace_commit_path("main"),
            Mode::Blob,
            oid('e').encode_line(),
        );
        let tree = builder.build(&mut store).unwrap();
        let bad = write_commit(
            &mut store,
            tree,
            vec![parent],
            Kind::RequestInterject.message(),
        );
        assert_reason(
            &store,
            &bad,
            "request.interject: re-applied tree differs first at path .caos/workspaces/main/commit",
        );
    }

    #[test]
    fn tool_pointer_must_match_resolution() {
        let (mut store, fork, _) = setup();
        let valid = find_commit(&store, &fork, |view| {
            view.kind() == Some(Kind::ToolComplete)
                && view.workspace("main").unwrap().unwrap().commit == oid('b')
        });
        let (parent, mut builder) = clone_delta(&mut store, &valid);
        builder.put(
            &paths::workspace_commit_path("main"),
            Mode::Blob,
            oid('d').encode_line(),
        );
        let tree = builder.build(&mut store).unwrap();
        let bad = write_commit(&mut store, tree, vec![parent], Kind::ToolComplete.message());
        assert_reason(
            &store,
            &bad,
            "tool.complete: re-applied tree differs first at path .caos/workspaces/main/commit",
        );
    }

    #[test]
    fn workspace_initial_may_not_change() {
        let (mut store, _, source) = setup();
        let bad = mutate(&mut store, &source, Kind::WorkspaceRollback, |builder| {
            builder.put(
                &paths::workspace_commit_path("main"),
                Mode::Blob,
                oid('d').encode_line(),
            );
            builder.put(
                &paths::workspace_initial_path("main"),
                Mode::Blob,
                oid('e').encode_line(),
            );
        });
        assert_reason(
            &store,
            &bad,
            "workspace.rollback: re-applied tree differs first at path .caos/workspaces/main/initial",
        );
    }

    #[test]
    fn workspace_directory_may_not_have_extra_files() {
        let (mut store, _, source) = setup();
        let bad = mutate(&mut store, &source, Kind::FilesApply, |builder| {
            builder.put(".caos/workspaces/main/extra", Mode::Blob, b"extra".to_vec());
        });
        assert_reason(&store, &bad, "files.apply requires at least one file");
    }

    #[test]
    fn request_claim_requires_queued_state() {
        let (mut store, fork, _) = setup();
        let parent = find_commit(&store, &fork, |view| {
            view.kind() == Some(Kind::RequestClaim)
                && view
                    .active_request()
                    .unwrap()
                    .is_some_and(|request| request.status == RequestStatus::Running)
        });
        let view = Conversation::open(&store, &parent).unwrap();
        let mut record = view.active_request().unwrap().unwrap();
        record.latest_message = Some("another-message".to_string());
        let path = paths::request_record_path(record.id.as_str());
        let bad = mutate(&mut store, &parent, Kind::RequestClaim, |builder| {
            builder.put(&path, Mode::Blob, record.encode());
        });
        assert_reason(&store, &bad, "request claim requires queued status");
    }

    #[test]
    fn request_admit_requires_no_active_request() {
        let (mut store, fork, _) = setup();
        let parent = find_commit(&store, &fork, |view| {
            view.active_request().unwrap().is_some() && view.kind() == Some(Kind::RequestClaim)
        });
        let id = oid('9');
        let record = request_record(id.clone(), parent.clone());
        let bad = mutate(&mut store, &parent, Kind::RequestAdmit, |builder| {
            builder.put(
                &paths::request_record_path(id.as_str()),
                Mode::Blob,
                record.encode(),
            );
            builder.put(paths::ACTIVE_REQUEST, Mode::Blob, id.encode_line());
        });
        assert_reason(&store, &bad, "another request is active");
    }

    #[test]
    fn tool_start_rejects_stale_input_workspace() {
        let (mut store, fork, _) = setup();
        let parent = find_first_model(&store, &fork);
        let request = Conversation::open(&store, &parent)
            .unwrap()
            .active_request()
            .unwrap()
            .unwrap();
        let record = started_tool(&request, "bash-call", Some(oid('e')));
        let path = paths::tool_record_path(request.id.as_str(), 0, "bash-call");
        let bad = mutate(&mut store, &parent, Kind::ToolStart, |builder| {
            builder.put(&path, Mode::Blob, record.encode());
        });
        assert_reason(&store, &bad, "tool input_workspace is stale");
    }

    #[test]
    fn tool_start_rejects_undeclared_call() {
        let (mut store, fork, _) = setup();
        let parent = find_first_model(&store, &fork);
        let request = Conversation::open(&store, &parent)
            .unwrap()
            .active_request()
            .unwrap()
            .unwrap();
        let record = started_tool(&request, "undeclared", None);
        let path = paths::tool_record_path(request.id.as_str(), 0, "undeclared");
        let bad = mutate(&mut store, &parent, Kind::ToolStart, |builder| {
            builder.put(&path, Mode::Blob, record.encode());
        });
        assert_reason(&store, &bad, "not declared");
    }

    #[test]
    fn root_may_not_contain_active_request() {
        let (mut store, fork, _) = setup();
        let root = find_kind(&store, &fork, Kind::ConversationRoot);
        let root_tree = store.read_commit(&root).unwrap().tree;
        let mut builder = TreeBuilder::from(Some(root_tree));
        builder.put(paths::ACTIVE_REQUEST, Mode::Blob, oid('9').encode_line());
        let tree = builder.build(&mut store).unwrap();
        let bad = write_commit(
            &mut store,
            tree,
            vec![g3()],
            Kind::ConversationRoot.message(),
        );
        assert_reason(
            &store,
            &bad,
            "conversation.root: re-applied tree differs first at path .caos/requests/active",
        );
    }

    #[test]
    fn fork_requires_fork_identity() {
        let (mut store, _, source) = setup();
        let identity = Identity {
            id: "bad-fork".to_string(),
            kind: IdentityKind::Root,
            owner: None,
        };
        let bad = mutate(&mut store, &source, Kind::ConversationFork, |builder| {
            builder.put(paths::IDENTITY, Mode::Blob, identity.encode());
        });
        assert_reason(&store, &bad, "fork");
    }

    #[test]
    fn fork_may_delete_only_running_children() {
        let (mut store, _, source) = setup();
        let view = Conversation::open(&store, &source).unwrap();
        let child = view
            .children()
            .unwrap()
            .into_iter()
            .find(|child| child.status == ChildStatus::Completed)
            .unwrap();
        let identity = Identity {
            id: "bad-fork".to_string(),
            kind: IdentityKind::Fork {
                source: source.clone(),
            },
            owner: None,
        };
        let bad = mutate(&mut store, &source, Kind::ConversationFork, |builder| {
            builder.put(paths::IDENTITY, Mode::Blob, identity.encode());
            builder.delete(&paths::subagent_record_path(&child.id));
        });
        assert_reason(&store, &bad, "kept it but child deleted it");
    }

    #[test]
    fn fork_rejects_active_or_cancelling_request() {
        let (mut store, fork, _) = setup();
        let source = find_commit(&store, &fork, |view| {
            view.kind() == Some(Kind::RequestAdmit) && view.active_request().unwrap().is_some()
        });
        let identity = Identity {
            id: "active-fork".to_string(),
            kind: IdentityKind::Fork {
                source: source.clone(),
            },
            owner: None,
        };
        let bad = mutate(&mut store, &source, Kind::ConversationFork, |builder| {
            builder.put(paths::IDENTITY, Mode::Blob, identity.encode());
        });
        assert_reason(
            &store,
            &bad,
            "cannot fork a conversation with an active or cancelling request",
        );
    }

    #[test]
    fn fork_rejects_started_tool() {
        let (mut store, fork, source) = setup();
        let started = find_kind(&store, &fork, Kind::ToolStart);
        let view = Conversation::open(&store, &started).unwrap();
        let request = view.active_request().unwrap().unwrap();
        let tool = view
            .tools(&request.id, request.round - 1)
            .unwrap()
            .remove(0);
        let path = paths::tool_record_path(tool.request.as_str(), tool.round, &tool.id);
        let source = source_with_record(&mut store, &source, &path, tool.encode());
        let bad = fork_without_apply(&mut store, &source, "started-tool-fork");
        assert_reason(&store, &bad, "started tool");
    }

    #[test]
    fn fork_rejects_nonterminal_async_task() {
        let (mut store, fork, source) = setup();
        let pending = find_kind(&store, &fork, Kind::AsyncStart);
        let task = Conversation::open(&store, &pending)
            .unwrap()
            .async_tasks()
            .unwrap()
            .remove(0);
        let path = paths::async_record_path(task.task.as_str());
        let source = source_with_record(&mut store, &source, &path, task.encode());
        let bad = fork_without_apply(&mut store, &source, "async-fork");
        assert_reason(&store, &bad, "nonterminal async task");
    }

    #[test]
    fn fork_rejects_nonterminal_publication() {
        let (mut store, fork, source) = setup();
        let pending = find_kind(&store, &fork, Kind::PublicationPending);
        let publication = Conversation::open(&store, &pending)
            .unwrap()
            .publications()
            .unwrap()
            .remove(0);
        let path = paths::publication_record_path(&publication.id);
        let source = source_with_record(&mut store, &source, &path, publication.encode());
        let bad = fork_without_apply(&mut store, &source, "publication-fork");
        assert_reason(&store, &bad, "nonterminal publication");
    }

    #[test]
    fn fork_must_remove_every_running_child() {
        let (mut store, fork, source) = setup();
        let spawned = find_kind(&store, &fork, Kind::SubagentSpawn);
        let child = Conversation::open(&store, &spawned)
            .unwrap()
            .children()
            .unwrap()
            .remove(0);
        let path = paths::subagent_record_path(&child.id);
        let source = source_with_record(&mut store, &source, &path, child.encode());
        let bad = fork_without_apply(&mut store, &source, "running-child-fork");
        assert_reason(&store, &bad, "deleted it but child kept it");
    }

    #[test]
    fn spawned_child_id_must_derive_from_call() {
        let (mut store, fork, _) = setup();
        let valid = find_kind(&store, &fork, Kind::SubagentSpawn);
        let valid_view = Conversation::open(&store, &valid).unwrap();
        let original = valid_view.children().unwrap().into_iter().next().unwrap();
        let original_path = paths::subagent_record_path(&original.id);
        let (parent, mut builder) = clone_delta(&mut store, &valid);
        builder.delete(&original_path);
        let mut wrong = original;
        wrong.id = "wrong-child".to_string();
        builder.put(
            &paths::subagent_record_path(&wrong.id),
            Mode::Blob,
            wrong.encode(),
        );
        let tree = builder.build(&mut store).unwrap();
        let bad = write_commit(
            &mut store,
            tree,
            vec![parent],
            Kind::SubagentSpawn.message(),
        );
        assert_reason(&store, &bad, "subagent child id mismatch");
    }

    #[test]
    fn publication_id_must_derive_from_record() {
        let (mut store, fork, _) = setup();
        let valid = find_kind(&store, &fork, Kind::PublicationPending);
        let valid_view = Conversation::open(&store, &valid).unwrap();
        let original = valid_view
            .publications()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let original_path = paths::publication_record_path(&original.id);
        let (parent, mut builder) = clone_delta(&mut store, &valid);
        builder.delete(&original_path);
        let mut wrong = original;
        wrong.id = "wrong-publication".to_string();
        builder.put(
            &paths::publication_record_path(&wrong.id),
            Mode::Blob,
            wrong.encode(),
        );
        let tree = builder.build(&mut store).unwrap();
        let bad = write_commit(
            &mut store,
            tree,
            vec![parent],
            Kind::PublicationPending.message(),
        );
        assert_reason(&store, &bad, "does not derive");
    }

    #[test]
    fn executable_blob_under_caos_is_malformed() {
        let (mut store, _, source) = setup();
        let bad = mutate(&mut store, &source, Kind::FilesApply, |builder| {
            builder.put(".caos/bad", Mode::Executable, b"bad".to_vec());
        });
        assert_reason(&store, &bad, "non-blob mode");
    }

    #[test]
    fn spine_reports_malformed_middle_commit() {
        let (mut store, _, source) = setup();
        let tree = store.read_commit(&source).unwrap().tree;
        let middle = write_commit(
            &mut store,
            tree.clone(),
            vec![source],
            Kind::MetadataTitleSet.message(),
        );
        let mut builder = TreeBuilder::from(Some(tree));
        builder.put("files/spine", Mode::Blob, b"head".to_vec());
        let head_tree = builder.build(&mut store).unwrap();
        let head = write_commit(
            &mut store,
            head_tree,
            vec![middle.clone()],
            Kind::FilesApply.message(),
        );
        let mut known = HashSet::new();
        let error = validate_spine(&store, &head, &mut known).unwrap_err();
        assert_eq!(error.commit, middle);
        assert!(
            known.is_empty(),
            "a failed walk must not populate the cache"
        );
        assert!(
            validate_spine(&store, &head, &mut known).is_err(),
            "a second validation accepted the same invalid spine"
        );
    }

    fn transcript_entry(
        ordinal: u64,
        message_id: &str,
        payload: Option<String>,
    ) -> TranscriptEntry {
        TranscriptEntry {
            message_id: message_id.to_string(),
            conversation: "golden-conversation".to_string(),
            role: Role::User,
            actor: "user".to_string(),
            request: None,
            round: None,
            model: None,
            blocks: payload
                .map(|path| vec![Block::Payload { path }])
                .unwrap_or_else(|| {
                    vec![Block::Text {
                        text: format!("message {ordinal}"),
                    }]
                }),
            proposal: None,
            workspace_resolution: None,
        }
    }

    fn find_first_model(store: &MemoryStore, head: &Oid) -> Oid {
        find_commit(store, head, |view| {
            view.kind() == Some(Kind::ModelComplete)
                && view.active_request().unwrap().is_some_and(|request| {
                    request.round == 1 && request.calls.iter().any(|call| call.id == "bash-call")
                })
        })
    }

    fn started_tool(request: &RequestRecord, id: &str, input_workspace: Option<Oid>) -> ToolRecord {
        ToolRecord {
            request: request.id.clone(),
            round: 0,
            id: id.to_string(),
            name: "bash".to_string(),
            declaration_message: "assistant-1".to_string(),
            workspace_name: input_workspace.as_ref().map(|_| "main".to_string()),
            input_workspace,
            status: ToolStatus::Started,
            task: Some(oid('9')),
            result: None,
            workspace_resolution: None,
            files: Vec::new(),
            files_outcome: None,
        }
    }
}
