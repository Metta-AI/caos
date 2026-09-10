use std::collections::HashSet;
use std::fmt;

use super::apply::{apply, Transition};
use super::canonical::parse_canonical;
use super::events::Event;
use super::kinds::Kind;
use super::oid::{empty_tree, g3, Oid};
use super::paths;
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
    if !info.extra_headers.is_empty() {
        return Err("conversation commits must not have extra headers".to_string());
    }
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
    let child = Conversation::open(store, commit)?;
    let parent_view = if kind == Kind::ConversationRoot {
        None
    } else {
        Some(
            Conversation::open(store, &parent)
                .map_err(|error| format!("parent tree is malformed: {error}"))?,
        )
    };

    let changes = diff(
        store,
        (kind != Kind::ConversationRoot).then_some(&parent_tree),
        &info.tree,
    )?;
    if changes.is_empty() && child.current_events()?.is_empty() {
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

    if kind == Kind::ConversationRoot {
        if !matches!(child.identity()?.kind, IdentityKind::Root)
            || !child.current_events()?.is_empty()
        {
            return Err("invalid root identity or execution history".into());
        }
        return Ok(Validated { parent, kind });
    }

    let transition = reconstruct(kind, parent_view.as_ref(), &child, &changes)?;
    let mut dry_run = DryRunStore::new(store);
    let applied = apply(
        &mut dry_run,
        (kind != Kind::ConversationRoot).then_some(&parent),
        &transition,
    )
    .map_err(|error| format!("{}: {error}", kind.as_str()))?;
    if applied.events != child.current_events()? {
        return Err("execution events do not match the transition".into());
    }
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
    let snapshot = super::tree::Snapshot::new(store, tree.clone());
    match snapshot.read(paths::FORMAT)? {
        Some(bytes) if bytes == paths::FORMAT_BYTES.as_bytes() => {}
        Some(_) => return Err("conversation format is invalid".to_string()),
        None => return Err("conversation format is missing".to_string()),
    }
    for entry in snapshot.list(".caos")? {
        if !matches!(
            entry.name.as_str(),
            "format" | "identity.json" | "title" | "transcript"
        ) {
            return Err(format!(
                "unknown conversation metadata .caos/{}",
                entry.name
            ));
        }
    }
    Conversation::open_tree(store, tree)?.identity()?;
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

        if let Some((mode, oid)) = &change.after {
            if under_caos && *mode != Mode::Blob {
                return Err(format!("non-blob mode under .caos at {}", change.path));
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
    path == paths::IDENTITY || transcript_record_path(path)
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
        Kind::ConversationRoot => Err("root is validated independently".into()),
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
        Kind::TurnAdmit => Ok(Transition::TurnAdmit {
            record: request_change(kind, child_snapshot, changes)?.1,
        }),
        Kind::TurnClaim => {
            let (request, record) = request_change(kind, child_snapshot, changes)?;
            let latest_message = record.latest_message.ok_or_else(|| {
                format!("{}: changed request has no latest_message", kind.as_str())
            })?;
            Ok(Transition::TurnClaim {
                request,
                latest_message,
            })
        }
        Kind::TurnInterject => {
            let (request, _) = request_change(kind, child_snapshot, changes)?;
            let (entry, payloads) = transcript_change(kind, child_snapshot, changes)?;
            Ok(Transition::TurnInterject {
                request,
                entry,
                payloads,
            })
        }
        Kind::TurnEscape => {
            let (request, record) = request_change(kind, child_snapshot, changes)?;
            Ok(Transition::TurnEscape {
                request,
                reason: record.escape_reason,
            })
        }
        Kind::TurnTerminal => {
            let (request, record) = request_change(kind, child_snapshot, changes)?;
            let outcome = record
                .outcome
                .ok_or_else(|| format!("{}: changed request has no outcome", kind.as_str()))?;
            Ok(Transition::TurnTerminal { request, outcome })
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
                paths::call_payload_dir(record.request.as_str(), record.round, &record.id);
            Ok(Transition::ToolComplete {
                payloads: payload_changes(kind, child_snapshot, changes, &payload_dir)?,
                files: file_changes(child_snapshot, changes)?
                    .into_iter()
                    .filter(|(path, _)| record.files.contains(path))
                    .collect(),
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
            let payload_dir = paths::call_payload_dir(tool.request.as_str(), tool.round, &tool.id);
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
            Ok(Transition::SubagentTerminal {
                child,
                terminal_head,
                status: record.status,
            })
        }
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
    _changes: &[Change],
) -> Result<(Oid, TurnRecord), String> {
    let records: Vec<_> = child
        .current_events()?
        .into_iter()
        .filter_map(|event| match event {
            Event::Request(r) => Some((r.id.clone(), r)),
            _ => None,
        })
        .collect();
    if records.len() != 1 {
        return Err(format!("{} requires one request event", kind.as_str()));
    }
    Ok(records.into_iter().next().unwrap())
}

fn tool_change(
    kind: Kind,
    child: &Conversation<'_>,
    _changes: &[Change],
) -> Result<(String, CallRecord), String> {
    let records: Vec<_> = child
        .current_events()?
        .into_iter()
        .filter_map(|event| match event {
            Event::Tool(r) => Some((r.id.clone(), r)),
            _ => None,
        })
        .collect();
    if records.len() != 1 {
        return Err(format!("{} requires one tool event", kind.as_str()));
    }
    Ok(records.into_iter().next().unwrap())
}

fn async_change(
    kind: Kind,
    child: &Conversation<'_>,
    _changes: &[Change],
) -> Result<(Oid, AsyncRecord), String> {
    let records: Vec<_> = child
        .current_events()?
        .into_iter()
        .filter_map(|event| match event {
            Event::Async(r) => Some((r.task.clone(), r)),
            _ => None,
        })
        .collect();
    if records.len() != 1 {
        return Err(format!("{} requires one async event", kind.as_str()));
    }
    Ok(records.into_iter().next().unwrap())
}

fn child_change(
    kind: Kind,
    child: &Conversation<'_>,
    _changes: &[Change],
) -> Result<(String, ChildRecord), String> {
    let records: Vec<_> = child
        .current_events()?
        .into_iter()
        .filter_map(|event| match event {
            Event::Child(r) => Some((r.id.clone(), r)),
            _ => None,
        })
        .collect();
    if records.len() != 1 {
        return Err(format!("{} requires one child event", kind.as_str()));
    }
    Ok(records.into_iter().next().unwrap())
}

fn publication_change(
    kind: Kind,
    child: &Conversation<'_>,
    _changes: &[Change],
) -> Result<(String, PublicationRecord), String> {
    let records: Vec<_> = child
        .current_events()?
        .into_iter()
        .filter_map(|event| match event {
            Event::Publication(r) => Some((r.id.clone(), r)),
            _ => None,
        })
        .collect();
    if records.len() != 1 {
        return Err(format!("{} requires one publication event", kind.as_str()));
    }
    Ok(records.into_iter().next().unwrap())
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

fn payload_changes(
    kind: Kind,
    child: &Conversation<'_>,
    changes: &[Change],
    directory: &str,
) -> Result<Payloads, String> {
    if directory.starts_with(paths::CALLS_DIR) {
        let prefix = format!("{directory}/");
        return Ok(child
            .current_events()?
            .into_iter()
            .filter_map(|event| match event {
                Event::Payload { path, bytes } => path
                    .strip_prefix(&prefix)
                    .map(|name| (name.to_string(), bytes)),
                _ => None,
            })
            .collect());
    }
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
        .filter(|change| !change.path.starts_with(".caos/"))
        .map(|change| (change.path.clone(), change))
        .map(|(relative, change)| {
            let value = match &change.after {
                Some((mode, _)) => Some((
                    *mode,
                    if matches!(mode, Mode::Commit | Mode::Tree) {
                        change.after.as_ref().unwrap().1.encode_line()
                    } else {
                        child
                            .snapshot()
                            .read(&change.path)?
                            .ok_or_else(|| format!("changed file {:?} is absent", change.path))?
                    },
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
    !rest.contains('/') && paths::parse_transcript_entry_path(path).is_ok()
}
