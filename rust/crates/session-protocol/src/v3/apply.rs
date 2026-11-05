use std::collections::BTreeSet;

use super::events::Event;
use super::ids;
use super::kinds::Kind;
use super::oid::Oid;
use super::paths;
use super::records::*;
use super::tree::{CommitInfo, Mode, ObjectStore, Signature, TreeBuilder};
use super::view::Session;

#[allow(clippy::large_enum_variant, clippy::type_complexity)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transition {
    SessionRoot {
        identity: Identity,
        title: String,
        content: Option<Oid>,
    },
    SessionFork {
        identity: Identity,
        title: String,
        /// Content the fork starts with INSTEAD of its source's, when it merged
        /// another commit in: a tree without `.caos/`, replacing every entry
        /// outside it. The commit it came from is the fork's second parent
        /// (`mint_with_parents`), which is what lets git find the base of the
        /// next merge. `None` keeps the source's content, as a plain fork does.
        content: Option<Oid>,
    },
    TitleSet {
        title: String,
    },
    MessageAppend {
        entry: TranscriptEntry,
        payloads: Vec<(String, Vec<u8>)>,
    },
    ToolStart {
        record: CallRecord,
        payloads: Vec<(String, Vec<u8>)>,
    },
    ToolComplete {
        record: CallRecord,
        payloads: Vec<(String, Vec<u8>)>,
        files: Vec<(String, Option<(Mode, Vec<u8>)>)>,
    },
    AsyncStart {
        record: AsyncRecord,
    },
    AsyncTerminal {
        task: Oid,
        status: TaskStatus,
        result: Option<Oid>,
        reason: Option<String>,
    },
    SubagentSpawn {
        tool: CallRecord,
        payloads: Vec<(String, Vec<u8>)>,
        child: ChildRecord,
    },
    SubagentTerminal {
        child: String,
        terminal_head: Oid,
        status: TaskStatus,
    },

    PublicationPending {
        record: PublicationRecord,
    },
    PublicationTerminal {
        publication: String,
        status: PublicationStatus,
        evidence: Evidence,
        observed: Option<Oid>,
    },
    FilesApply {
        files: Vec<(String, Option<(Mode, Vec<u8>)>)>,
    },
}

impl Transition {
    pub fn reference(name: String, commit: Option<Oid>) -> Self {
        Self::FilesApply {
            files: vec![(name, commit.map(|oid| (Mode::Commit, oid.encode_line())))],
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Transition::SessionRoot { .. } => Kind::SessionRoot,
            Transition::SessionFork { .. } => Kind::SessionFork,
            Transition::TitleSet { .. } => Kind::MetadataTitleSet,
            Transition::MessageAppend { .. } => Kind::MessageAppend,
            Transition::ToolStart { .. } => Kind::ToolStart,
            Transition::ToolComplete { .. } => Kind::ToolComplete,
            Transition::AsyncStart { .. } => Kind::AsyncStart,
            Transition::AsyncTerminal { .. } => Kind::AsyncTerminal,
            Transition::SubagentSpawn { .. } => Kind::SubagentSpawn,
            Transition::SubagentTerminal { .. } => Kind::SubagentTerminal,
            Transition::PublicationPending { .. } => Kind::PublicationPending,
            Transition::PublicationTerminal { .. } => Kind::PublicationTerminal,
            Transition::FilesApply { .. } => Kind::FilesApply,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub tree: Oid,
    pub ordinal: Option<u64>,
    pub events: Vec<Event>,
}

pub fn apply(
    store: &mut dyn ObjectStore,
    parent_head: Option<&Oid>,
    transition: &Transition,
) -> Result<Applied, String> {
    if matches!(transition, Transition::SessionRoot { .. }) {
        if parent_head.is_some() {
            return Err("session root requires no parent tree".to_string());
        }
    } else if parent_head.is_none() {
        return Err(format!(
            "{} requires a parent tree",
            transition.kind().as_str()
        ));
    }
    let parent_tree = parent_head
        .map(|head| store.read_commit(head).map(|info| info.tree))
        .transpose()
        .map_err(String::from)?;
    let mut builder = TreeBuilder::from(parent_tree);
    let mut events = Vec::new();
    let mut ordinal = None;

    match transition {
        Transition::SessionRoot {
            identity,
            title,
            content,
        } => {
            canonical_shape(identity, "identity")?;
            if !matches!(identity.kind, IdentityKind::Root) {
                return Err("session root identity must have root kind".to_string());
            }
            builder.put(
                paths::FORMAT,
                Mode::Blob,
                paths::FORMAT_BYTES.as_bytes().to_vec(),
            );
            builder.put(paths::IDENTITY, Mode::Blob, identity.content_bytes());
            builder.put(paths::TITLE, Mode::Blob, encode_title(title)?);
            if let Some(seed) = content {
                for entry in super::tree::Snapshot::new(store, seed.clone()).list("")? {
                    if entry.name == ".caos" {
                        return Err("content seed must not contain .caos".into());
                    }
                    builder.put_oid(&entry.name, entry.mode, entry.oid);
                }
            }
        }
        Transition::SessionFork {
            identity,
            title,
            content,
        } => {
            let session = parent(store, parent_head)?;
            canonical_shape(identity, "identity")?;
            if !matches!(identity.kind, IdentityKind::Fork { .. }) {
                return Err("session fork identity must have fork kind".to_string());
            }
            if identity.id == session.identity()?.id {
                return Err("session fork identity id must differ from its source".to_string());
            }
            validate_fork_quiescence(&session)?;
            if let Some(merged) = content {
                // Everything outside .caos/ is replaced by the merged tree's
                // entries. .caos/ is the fork's own record, never merged.
                let incoming = super::tree::Snapshot::new(store, merged.clone()).list("")?;
                if incoming.iter().any(|entry| entry.name == paths::CAOS_DIR) {
                    return Err("fork content must not contain .caos".to_string());
                }
                // Every one of them, including those the merge keeps: a put onto
                // a directory the source has is refused unless that path was
                // explicitly deleted first (`TreeBuilder`).
                for entry in session.snapshot().list("")? {
                    if entry.name != paths::CAOS_DIR {
                        builder.delete(&entry.name);
                    }
                }
                for entry in incoming {
                    builder.put_oid(&entry.name, entry.mode, entry.oid);
                }
            }
            // Forking a v6 session (recorded before the rename) makes a v7
            // one: the history it starts from is re-spelled the v7 way, so
            // nothing written after the fork has to know about v6.
            if session.format()? == paths::Format::V6 {
                builder.put(paths::FORMAT, Mode::Blob, paths::FORMAT_BYTES.as_bytes().to_vec());
                let len = session.transcript_len()?;
                for (ordinal, message_id, entry) in session.transcript(0, len)? {
                    builder.put(
                        &paths::transcript_entry_path(ordinal, &message_id),
                        Mode::Blob,
                        entry.encode_as(paths::Format::V7),
                    );
                }
            }
            builder.put(paths::IDENTITY, Mode::Blob, identity.content_bytes());
            builder.put(paths::TITLE, Mode::Blob, encode_title(title)?);
        }
        Transition::TitleSet { title } => {
            let session = parent(store, parent_head)?;
            if session.title()? == *title {
                return Err("no-op".to_string());
            }
            builder.put(paths::TITLE, Mode::Blob, encode_title(title)?);
        }
        Transition::MessageAppend { entry, payloads } => {
            let session = parent(store, parent_head)?;
            if entry.role == Role::Assistant {
                validate_declared_calls(&session, entry)?;
            } else if entry.tool_uses().next().is_some() {
                return Err("only an assistant message declares tool calls".to_string());
            }
            let next = session.transcript_len()?;
            validate_entry_resolution(&session, entry, &mut builder)?;
            put_transcript(&mut builder, session.format()?, next, entry, payloads)?;
            ordinal = Some(next);
        }
        Transition::ToolStart { record, payloads } => {
            let session = parent(store, parent_head)?;
            canonical_shape(record, "tool")?;
            if record.status != CallStatus::Started {
                return Err("tool.start requires started status".to_string());
            }
            if session.tool(&record.id)?.is_some() {
                return Err(format!("tool {:?} already exists", record.id));
            }
            validate_tool_source_tree(&session, record)?;
            validate_new_tool_payloads(&session, record, payloads)?;
            put_tool_payloads(&mut events, record, payloads)?;
            put_tool(&mut events, record);
        }
        Transition::ToolComplete {
            record,
            payloads,
            files,
        } => {
            let session = parent(store, parent_head)?;
            validate_tool_completion(&session, record, payloads, files, &mut builder, &mut events)?;
        }
        Transition::AsyncStart { record } => {
            let session = parent(store, parent_head)?;
            canonical_shape(record, "async record")?;
            if record.status != TaskStatus::Pending {
                return Err("async.start requires pending status".to_string());
            }
            if session.async_task(&record.task)?.is_some() {
                return Err(format!("async task {} already exists", record.task));
            }
            put_async(&mut events, record);
        }
        Transition::AsyncTerminal {
            task,
            status,
            result,
            reason,
        } => {
            let session = parent(store, parent_head)?;
            let mut record = session
                .async_task(task)?
                .ok_or_else(|| format!("async task {task} does not exist"))?;
            record.status = record.status.finish(*status)?;
            record.result = result.clone();
            record.reason = reason.clone();
            canonical_shape(&record, "async record")?;
            put_async(&mut events, &record);
        }
        Transition::SubagentSpawn {
            tool,
            payloads,
            child,
        } => {
            let session = parent(store, parent_head)?;
            canonical_shape(tool, "tool")?;
            canonical_shape(child, "child")?;
            if session.tool(&tool.id)?.is_some() {
                return Err(format!("tool {:?} already exists", tool.id));
            }
            let Some(ToolResult::Complete {
                observation,
                proposal: None,
            }) = &tool.result
            else {
                return Err(
                    "subagent spawn requires a startless complete tool without a proposal"
                        .to_string(),
                );
            };
            if tool.task.as_ref().is_some_and(|task| task != &child.relay)
                || tool.status != CallStatus::Complete
                || tool.source_tree_resolution.is_some()
                || !tool.files.is_empty()
                || tool.files_outcome.is_some()
            {
                return Err(
                    "subagent spawn requires a startless complete tool without a proposal"
                        .to_string(),
                );
            }
            if child.status != TaskStatus::Pending {
                return Err("subagent spawn requires a running child".to_string());
            }
            let expected = ids::child_id(&session.identity()?.id, &tool.id)?;
            if child.id != expected {
                return Err(format!(
                    "subagent child id mismatch: got {:?}, expected {expected:?}",
                    child.id
                ));
            }
            if child.spawn_intent.tool != tool.id {
                return Err("subagent spawn intent does not match tool".to_string());
            }
            if session.child(&child.id)?.is_some() {
                return Err(format!("subagent {:?} already exists", child.id));
            }
            validate_new_tool_payloads(&session, tool, payloads)?;
            let payload_paths = put_tool_payloads(&mut events, tool, payloads)?;
            if !payload_paths.contains(observation) {
                return Err(
                    "subagent tool observation is not supplied by this transition".to_string(),
                );
            }
            put_tool(&mut events, tool);
            put_child(&mut events, child);
        }
        Transition::SubagentTerminal {
            child,
            terminal_head,
            status,
        } => {
            let session = parent(store, parent_head)?;
            let mut record = require_child(&session, child)?;
            record.status = record.status.finish(*status)?;
            record.terminal_head = Some(terminal_head.clone());
            canonical_shape(&record, "child record")?;
            put_child(&mut events, &record);
        }
        Transition::PublicationPending { record } => {
            let session = parent(store, parent_head)?;
            canonical_shape(record, "publication")?;
            paths::validate_protocol_id_component(&record.id)?;
            if record.status != PublicationStatus::Pending {
                return Err("publication.pending requires pending status".to_string());
            }
            if session.publication(&record.id)?.is_some() {
                return Err(format!("publication {:?} already exists", record.id));
            }
            let projection = ids::projection_id(&record.descriptor.to_value())?;
            let expected = ids::publication_id(
                &session.identity()?.id,
                &record.key,
                &projection,
                &record.planned_head,
                &record.repository,
                &record.refname,
                record.expected_old.as_ref(),
            )?;
            if record.id != expected {
                return Err("publication id does not derive from its record".to_string());
            }
            put_publication(&mut events, record);
        }
        Transition::PublicationTerminal {
            publication,
            status,
            evidence,
            observed,
        } => {
            let session = parent(store, parent_head)?;
            let mut record = session
                .publication(publication)?
                .ok_or_else(|| format!("publication {publication:?} does not exist"))?;
            if record.status != PublicationStatus::Pending {
                return Err("publication terminal requires pending status".to_string());
            }
            if *status == PublicationStatus::Pending {
                return Err("publication terminal status must be terminal".to_string());
            }
            Evidence::from_value(&evidence.to_value())?;
            record.status = *status;
            record.evidence = Some(evidence.clone());
            record.observed = observed.clone();
            put_publication(&mut events, &record);
        }
        Transition::FilesApply { files } => {
            let session = parent(store, parent_head)?;
            if files.is_empty() {
                return Err("files.apply requires at least one file".to_string());
            }
            apply_files(&session, &mut builder, files, "file path")?;
        }
    }

    Ok(Applied {
        tree: builder.build(store)?,
        ordinal,
        events,
    })
}

fn validate_fork_quiescence(_session: &Session<'_>) -> Result<(), String> {
    // A session is a flat transcript; a fork copies it as it stands, with
    // no outstanding-work check. A call that started and never finished is just
    // a tool_use with no tool_result, which a reader renders as unfinished and a
    // resuming driver decides what to do with. (Formerly refused an active
    // request, a started tool, or a pending async task or publication, which is
    // why a session that died mid-turn had to be "settled" before it could fork.)
    Ok(())
}

pub fn mint(
    store: &mut dyn ObjectStore,
    parent: &Oid,
    applied: &Applied,
    kind: Kind,
    signature: &Signature,
) -> Result<Oid, String> {
    mint_with_parents(store, parent, &[], applied, kind, signature)
}

/// Mint with further parents after the spine's `parent`: the commit a fork
/// merged its content from. The first parent is always the session, which
/// is how every reader walks it; the others are there so that git can find a
/// merge base the next time that repository's newer commit is merged in.
///
/// A root's parent is G3, or the commit whose tree is its content -- with the
/// same purpose. Neither place takes a session commit: that would splice
/// one session's history into another.
pub fn mint_with_parents(
    store: &mut dyn ObjectStore,
    parent: &Oid,
    merged: &[Oid],
    applied: &Applied,
    kind: Kind,
    signature: &Signature,
) -> Result<Oid, String> {
    if kind == Kind::SessionRoot {
        if parent != &super::oid::g3() && is_session_commit(store, parent)? {
            return Err(
                "a session root parents G3 or the commit its content came from, \
                 not a session commit"
                    .to_string(),
            );
        }
    } else if parent == &super::oid::g3() {
        return Err("only a session root may parent G3".to_string());
    }
    if !merged.is_empty() && kind != Kind::SessionFork {
        return Err("only a session fork merges another commit in".to_string());
    }
    for commit in merged {
        if is_session_commit(store, commit)? {
            return Err("a fork merges a content commit in, not a session commit".to_string());
        }
    }
    store
        .write_commit(&CommitInfo {
            tree: applied.tree.clone(),
            parents: std::iter::once(parent).chain(merged).cloned().collect(),
            author: signature.clone(),
            committer: signature.clone(),
            extra_headers: Vec::new(),
            message: super::events::encode(kind, &applied.events),
        })
        .map_err(String::from)
}

/// Whether a commit's message names a session transition, which is what
/// makes it part of a session rather than content.
pub fn is_session_commit(store: &dyn ObjectStore, commit: &Oid) -> Result<bool, String> {
    let info = store.read_commit(commit).map_err(String::from)?;
    Ok(Kind::parse_message(&info.message).is_ok())
}

pub fn inherited_signature(store: &dyn ObjectStore, parent: &Oid) -> Result<Signature, String> {
    store
        .read_commit(parent)
        .map(|commit| commit.committer)
        .map_err(String::from)
}

pub fn client_signature(name: &str, email: &str, unix_time: i64) -> Signature {
    Signature {
        name: name.to_string(),
        email: email.to_string(),
        time: unix_time,
        offset: "+0000".to_string(),
    }
}

fn parent<'s>(store: &'s dyn ObjectStore, tree: Option<&Oid>) -> Result<Session<'s>, String> {
    Session::open(store, tree.expect("non-root transition has parent commit"))
}

fn canonical_shape<T: Record + PartialEq>(record: &T, what: &str) -> Result<(), String> {
    if T::from_value(&record.to_value())? != *record {
        return Err(format!("{what} does not have a canonical record shape"));
    }
    Ok(())
}

fn put_tool(events: &mut Vec<Event>, record: &CallRecord) {
    events.push(Event::Tool(record.clone()));
}

fn put_async(events: &mut Vec<Event>, record: &AsyncRecord) {
    events.push(Event::Async(record.clone()));
}

fn put_child(events: &mut Vec<Event>, record: &ChildRecord) {
    events.push(Event::Child(record.clone()));
}

fn put_publication(events: &mut Vec<Event>, record: &PublicationRecord) {
    events.push(Event::Publication(record.clone()));
}

fn put_transcript(
    builder: &mut TreeBuilder,
    format: paths::Format,
    ordinal: u64,
    entry: &TranscriptEntry,
    payloads: &[(String, Vec<u8>)],
) -> Result<(), String> {
    TranscriptEntry::from_value(&entry.to_value())?;
    paths::validate_protocol_id_component(&entry.message_id)?;
    let payload_paths = put_payloads(
        builder,
        &paths::transcript_payload_dir(ordinal, &entry.message_id),
        payloads,
    )?;
    validate_entry_payloads(entry, &payload_paths)?;
    builder.put(
        &paths::transcript_entry_path(ordinal, &entry.message_id),
        Mode::Blob,
        entry.encode_as(format),
    );
    Ok(())
}

fn put_payloads(
    builder: &mut TreeBuilder,
    dir: &str,
    payloads: &[(String, Vec<u8>)],
) -> Result<BTreeSet<String>, String> {
    let mut paths_set = BTreeSet::new();
    for (name, bytes) in payloads {
        paths::validate_component(name)?;
        let full_path = format!("{dir}/{name}");
        if !paths_set.insert(full_path.clone()) {
            return Err(format!("duplicate payload name {name:?}"));
        }
        builder.put(&full_path, Mode::Blob, bytes.clone());
    }
    Ok(paths_set)
}

fn validate_entry_payloads(
    entry: &TranscriptEntry,
    payload_paths: &BTreeSet<String>,
) -> Result<(), String> {
    for block in &entry.blocks {
        let referenced = match block {
            Block::Payload { path } => Some(path),
            Block::ToolUse { arguments, .. } => Some(arguments),
            Block::Text { .. } => None,
        };
        if let Some(path) = referenced {
            if !payload_paths.contains(path) {
                return Err(format!(
                    "transcript payload reference {path:?} is not supplied by this transition"
                ));
            }
        }
    }
    Ok(())
}

/// An assistant message's `tool_use` ids are the ids its calls are recorded
/// under, so they are unique: within the message, and against every call the
/// session has already recorded.
fn validate_declared_calls(session: &Session<'_>, entry: &TranscriptEntry) -> Result<(), String> {
    let ids: Vec<String> = entry.tool_uses().map(|(id, _, _)| id.to_string()).collect();
    paths::validate_unique(&ids, "declared call id")?;
    for id in &ids {
        if session.tool(id)?.is_some() {
            return Err(format!("call {id:?} is already recorded"));
        }
    }
    Ok(())
}

fn validate_entry_resolution(
    session: &Session<'_>,
    entry: &TranscriptEntry,
    builder: &mut TreeBuilder,
) -> Result<(), String> {
    let proposal = entry
        .proposal
        .as_ref()
        .map(|proposal| {
            let source_tree = session
                .source_tree(&proposal.source_tree_name)?
                .ok_or_else(|| {
                    format!("source tree {:?} does not exist", proposal.source_tree_name)
                })?;
            Ok::<(&Proposal, SourceTreeRecord), String>((proposal, source_tree))
        })
        .transpose()?;
    let Some(resolution) = &entry.source_tree_resolution else {
        return Ok(());
    };
    let (proposal, source_tree) =
        proposal.ok_or_else(|| "source tree resolution requires a proposal".to_string())?;
    move_source_tree_pointer(
        builder,
        &proposal.source_tree_name,
        &source_tree.commit,
        resolution.new_pointer(),
        "source tree pointer update",
    )?;
    Ok(())
}

fn validate_tool_source_tree(session: &Session<'_>, record: &CallRecord) -> Result<(), String> {
    if record.source_tree_name.is_some() && record.input_commit.is_none() {
        return Err("a selected source tree requires an input commit".to_string());
    }
    if record.source_tree_name.is_none() {
        if let Some(input) = &record.input_commit {
            if session.commit() != Some(input) {
                return Err("tool input session is stale".into());
            }
        }
    }
    if let (Some(name), Some(input)) = (&record.source_tree_name, &record.input_commit) {
        let source_tree = session
            .source_tree(name)?
            .ok_or_else(|| format!("source tree {name:?} does not exist"))?;
        if source_tree.commit != *input {
            return Err("tool input_commit is stale".to_string());
        }
    }
    Ok(())
}

#[allow(clippy::type_complexity)]
fn validate_tool_completion(
    session: &Session<'_>,
    record: &CallRecord,
    payloads: &[(String, Vec<u8>)],
    files: &[(String, Option<(Mode, Vec<u8>)>)],
    builder: &mut TreeBuilder,
    events: &mut Vec<Event>,
) -> Result<(), String> {
    canonical_shape(record, "tool")?;
    if record.status == CallStatus::Started {
        return Err("tool.complete requires a terminal status".to_string());
    }
    let existing = session.tool(&record.id)?;
    if let Some(started) = &existing {
        if started.status != CallStatus::Started {
            return Err("tool.complete requires an absent or started tool record".to_string());
        }
        if started.name != record.name
            || started.declaration_message != record.declaration_message
            || started.source_tree_name != record.source_tree_name
            || started.input_commit != record.input_commit
            || started.task != record.task
        {
            return Err("tool.complete identity fields do not match tool.start".to_string());
        }
    } else if let Some(task) = &record.task {
        if session.task(task)?.is_none() {
            return Err("startless tool.complete must refer to an existing task".to_string());
        }
    }
    if existing.is_none() {
        validate_tool_source_tree(session, record)?;
    }
    let result = record
        .result
        .as_ref()
        .ok_or_else(|| "tool.complete requires a result".to_string())?;
    if CallRecord::expected_status(result, record.source_tree_resolution.as_ref()) != record.status
    {
        return Err("tool status does not match result and resolution".to_string());
    }
    validate_new_tool_payloads(session, record, payloads)?;
    let payload_paths = put_tool_payloads(events, record, payloads)?;
    let result_path = match result {
        ToolResult::Complete { observation, .. } => Some(observation),
        ToolResult::Failed { error } => Some(error),
        ToolResult::Cancelled { .. } => None,
    };
    if let Some(path) = result_path {
        if !payload_paths.contains(path) {
            return Err(format!(
                "tool result path {path:?} is not supplied by this transition"
            ));
        }
    }
    let mut supplied: Vec<String> = files.iter().map(|(path, _)| path.clone()).collect();
    supplied.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    if supplied != record.files {
        return Err("tool.complete files do not match record.files".to_string());
    }
    apply_files(session, builder, files, "tool file path")?;
    if let Some(resolution) = &record.source_tree_resolution {
        let name = record
            .source_tree_name
            .as_ref()
            .ok_or_else(|| "tool source tree resolution requires source_tree_name".to_string())?;
        let source_tree = session
            .source_tree(name)?
            .ok_or_else(|| format!("source tree {name:?} does not exist"))?;
        move_source_tree_pointer(
            builder,
            name,
            &source_tree.commit,
            resolution.new_pointer(),
            "tool source tree pointer update",
        )?;
    }
    put_tool(events, record);
    Ok(())
}

fn move_source_tree_pointer(
    builder: &mut TreeBuilder,
    name: &str,
    current: &Oid,
    pointer: Option<&Oid>,
    what: &str,
) -> Result<(), String> {
    let Some(pointer) = pointer else {
        return Ok(());
    };
    if pointer == current {
        return Err(format!("{what} is a no-op"));
    }
    builder.put_oid(name, Mode::Commit, pointer.clone());
    Ok(())
}

fn put_tool_payloads(
    events: &mut Vec<Event>,
    record: &CallRecord,
    payloads: &[(String, Vec<u8>)],
) -> Result<BTreeSet<String>, String> {
    let dir = paths::call_payload_dir(&record.id);
    let mut paths = BTreeSet::new();
    for (name, bytes) in payloads {
        super::paths::validate_component(name)?;
        let path = format!("{dir}/{name}");
        if !paths.insert(path.clone()) {
            return Err("duplicate tool payload".into());
        }
        events.push(Event::Payload {
            path,
            bytes: bytes.clone(),
        });
    }
    Ok(paths)
}

fn validate_new_tool_payloads(
    session: &Session<'_>,
    record: &CallRecord,
    payloads: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let dir = paths::call_payload_dir(&record.id);
    for (name, _) in payloads {
        paths::validate_component(name)?;
        let path = format!("{dir}/{name}");
        if session.snapshot().exists(&path)? {
            return Err(format!("tool payload {path:?} already exists"));
        }
    }
    Ok(())
}

#[allow(clippy::type_complexity)]
fn apply_files(
    session: &Session<'_>,
    builder: &mut TreeBuilder,
    files: &[(String, Option<(Mode, Vec<u8>)>)],
    unique_what: &str,
) -> Result<(), String> {
    let names: Vec<String> = files.iter().map(|(path, _)| path.clone()).collect();
    paths::validate_unique(&names, unique_what)?;
    for (relative, value) in files {
        paths::validate_source_tree_name(relative)?;
        let path = paths::files_path(relative);
        let before = session.snapshot().entry(&path)?;
        let changed = match (before, value) {
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
            (Some(entry), Some((mode, bytes))) => {
                entry.mode != *mode
                    || if matches!(mode, Mode::Commit | Mode::Tree) {
                        entry.oid.encode_line() != *bytes
                    } else {
                        session.snapshot().read(&path)?.as_deref() != Some(bytes.as_slice())
                    }
            }
        };
        if !changed {
            continue;
        }
        match value {
            Some((mode @ (Mode::Blob | Mode::Executable | Mode::Link), bytes)) => {
                builder.put(&path, *mode, bytes.clone());
            }
            Some((mode @ (Mode::Tree | Mode::Commit), bytes)) => {
                builder.put_oid(&path, *mode, Oid::parse_line(bytes, "content object")?);
            }
            None => builder.delete(&path),
        }
    }
    Ok(())
}

fn require_child(session: &Session<'_>, child: &str) -> Result<ChildRecord, String> {
    session
        .child(child)?
        .ok_or_else(|| format!("subagent {child:?} does not exist"))
}
