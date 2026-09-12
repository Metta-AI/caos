use std::collections::BTreeSet;

use super::events::Event;
use super::ids;
use super::kinds::Kind;
use super::oid::Oid;
use super::paths;
use super::records::*;
use super::tree::{CommitInfo, Mode, ObjectStore, Signature, TreeBuilder};
use super::view::Conversation;

#[allow(clippy::large_enum_variant, clippy::type_complexity)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transition {
    ConversationRoot {
        identity: Identity,
        title: String,
        content: Option<Oid>,
    },
    ConversationFork {
        identity: Identity,
        title: String,
    },
    TitleSet {
        title: String,
    },
    MessageAppend {
        entry: TranscriptEntry,
        payloads: Vec<(String, Vec<u8>)>,
    },
    TurnAdmit {
        record: TurnRecord,
    },
    TurnClaim {
        request: Oid,
        latest_message: String,
    },
    TurnInterject {
        request: Oid,
        entry: TranscriptEntry,
        payloads: Vec<(String, Vec<u8>)>,
    },
    TurnEscape {
        request: Oid,
        reason: Option<String>,
    },
    TurnTerminal {
        request: Oid,
        outcome: TurnOutcome,
    },
    ModelComplete {
        request: Oid,
        entry: TranscriptEntry,
        payloads: Vec<(String, Vec<u8>)>,
        calls: Vec<DeclaredCall>,
    },
    ToolStart {
        record: CallRecord,
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
            Transition::ConversationRoot { .. } => Kind::ConversationRoot,
            Transition::ConversationFork { .. } => Kind::ConversationFork,
            Transition::TitleSet { .. } => Kind::MetadataTitleSet,
            Transition::MessageAppend { .. } => Kind::MessageAppend,
            Transition::TurnAdmit { .. } => Kind::TurnAdmit,
            Transition::TurnClaim { .. } => Kind::TurnClaim,
            Transition::TurnInterject { .. } => Kind::TurnInterject,
            Transition::TurnEscape { .. } => Kind::TurnEscape,
            Transition::TurnTerminal { .. } => Kind::TurnTerminal,
            Transition::ModelComplete { .. } => Kind::ModelComplete,
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
    if matches!(transition, Transition::ConversationRoot { .. }) {
        if parent_head.is_some() {
            return Err("conversation root requires no parent tree".to_string());
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
        Transition::ConversationRoot {
            identity,
            title,
            content,
        } => {
            canonical_shape(identity, "identity")?;
            if !matches!(identity.kind, IdentityKind::Root) {
                return Err("conversation root identity must have root kind".to_string());
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
        Transition::ConversationFork { identity, title } => {
            let conversation = parent(store, parent_head)?;
            canonical_shape(identity, "identity")?;
            if !matches!(identity.kind, IdentityKind::Fork { .. }) {
                return Err("conversation fork identity must have fork kind".to_string());
            }
            if identity.id == conversation.identity()?.id {
                return Err("conversation fork identity id must differ from its source".to_string());
            }
            validate_fork_quiescence(&conversation)?;
            builder.put(paths::IDENTITY, Mode::Blob, identity.content_bytes());
            builder.put(paths::TITLE, Mode::Blob, encode_title(title)?);
        }
        Transition::TitleSet { title } => {
            let conversation = parent(store, parent_head)?;
            if conversation.title()? == *title {
                return Err("no-op".to_string());
            }
            builder.put(paths::TITLE, Mode::Blob, encode_title(title)?);
        }
        Transition::MessageAppend { entry, payloads } => {
            let conversation = parent(store, parent_head)?;
            if !matches!(entry.role, Role::User | Role::System) {
                return Err("message.append requires a user or system entry".to_string());
            }
            let next = conversation.transcript_len()?;
            validate_entry_resolution(&conversation, entry, &mut builder)?;
            put_transcript(&mut builder, next, entry, payloads)?;
            ordinal = Some(next);
        }
        Transition::TurnAdmit { record } => {
            let conversation = parent(store, parent_head)?;
            canonical_shape(record, "request")?;
            if conversation.active_turn()?.is_some() {
                return Err("cannot admit a request while another request is active".to_string());
            }
            if conversation.turn(&record.id)?.is_some() {
                return Err(format!("request {} already exists", record.id));
            }
            if record.status != TurnStatus::Queued
                || record.round != 0
                || !record.calls.is_empty()
                || !record.interjections.is_empty()
            {
                return Err(
                    "admitted request must be queued at round zero with no calls or interjections"
                        .to_string(),
                );
            }
            put_request(&mut events, record);
        }
        Transition::TurnClaim {
            request,
            latest_message,
        } => {
            let conversation = parent(store, parent_head)?;
            let mut record = require_active(&conversation, request)?;
            if record.status != TurnStatus::Queued {
                return Err("request claim requires queued status".to_string());
            }
            paths::validate_protocol_id_component(latest_message)?;
            record.status = TurnStatus::Running;
            record.latest_message = Some(latest_message.clone());
            put_request(&mut events, &record);
        }
        Transition::TurnInterject {
            request,
            entry,
            payloads,
        } => {
            let conversation = parent(store, parent_head)?;
            let mut record = require_active(&conversation, request)?;
            if !matches!(
                record.status,
                TurnStatus::Queued | TurnStatus::Running | TurnStatus::Cancelling
            ) {
                return Err("request interject requires an active request status".to_string());
            }
            if entry.role != Role::User {
                return Err("request interject requires a user entry".to_string());
            }
            let next = conversation.transcript_len()?;
            validate_entry_resolution(&conversation, entry, &mut builder)?;
            put_transcript(&mut builder, next, entry, payloads)?;
            record.interjections.push(entry.message_id.clone());
            if matches!(record.status, TurnStatus::Running | TurnStatus::Cancelling) {
                record.latest_message = Some(entry.message_id.clone());
            }
            put_request(&mut events, &record);
            ordinal = Some(next);
        }
        Transition::TurnEscape { request, reason } => {
            let conversation = parent(store, parent_head)?;
            let mut record = require_active(&conversation, request)?;
            match record.status {
                TurnStatus::Queued => {
                    record.status = TurnStatus::Idle;
                    record.escape_reason = reason.clone();
                    record.outcome = Some(TurnOutcome::Idle {
                        result: None,
                        interrupted: true,
                    });
                }
                TurnStatus::Running => {
                    record.status = TurnStatus::Cancelling;
                    record.escape_reason = reason.clone();
                }
                _ => return Err("request escape requires queued or running status".to_string()),
            }
            put_request(&mut events, &record);
        }
        Transition::TurnTerminal { request, outcome } => {
            let conversation = parent(store, parent_head)?;
            let mut record = require_active(&conversation, request)?;
            if !matches!(record.status, TurnStatus::Running | TurnStatus::Cancelling) {
                return Err("request terminal requires running or cancelling status".to_string());
            }
            if record.status == TurnStatus::Cancelling && record.round != 0 {
                for call in &record.calls {
                    let tool = conversation.tool(&record.id, record.round - 1, &call.id)?;
                    if !tool.is_some_and(|tool| tool.is_terminal()) {
                        return Err(
                            "cancelling request terminated with an outstanding call".to_string()
                        );
                    }
                }
            }
            record.status = match outcome {
                TurnOutcome::Idle {
                    result: Some(path), ..
                } => {
                    validate_caos_path(path, "request result path")?;
                    TurnStatus::Idle
                }
                TurnOutcome::Idle { .. } => TurnStatus::Idle,
                TurnOutcome::Failed { error } => {
                    validate_caos_path(error, "request error path")?;
                    record.escape_reason = None;
                    TurnStatus::Failed
                }
            };
            record.latest_message = None;
            record.outcome = Some(outcome.clone());
            put_request(&mut events, &record);
        }
        Transition::ModelComplete {
            request,
            entry,
            payloads,
            calls,
        } => {
            let conversation = parent(store, parent_head)?;
            let mut record = require_active(&conversation, request)?;
            if record.status != TurnStatus::Running {
                return Err("model.complete requires running request status".to_string());
            }
            if entry.role != Role::Assistant
                || entry.request.as_ref() != Some(request)
                || entry.round != Some(record.round)
            {
                return Err("model.complete entry has wrong role, request, or round".to_string());
            }
            let declared_ids: Vec<String> = calls.iter().map(|call| call.id.clone()).collect();
            let block_ids: Vec<DeclaredCall> = entry
                .blocks
                .iter()
                .filter_map(|block| match block {
                    Block::ToolUse { id, name, .. } => Some(DeclaredCall {
                        id: id.clone(),
                        name: name.clone(),
                    }),
                    _ => None,
                })
                .collect();
            paths::validate_unique(&declared_ids, "declared call id")?;
            if *calls != block_ids {
                return Err("model.complete calls do not match tool_use blocks".to_string());
            }
            let next = conversation.transcript_len()?;
            put_transcript(&mut builder, next, entry, payloads)?;
            if record.round == paths::MAX_JSON_INT {
                return Err("request round exceeds the maximum JSON integer".to_string());
            }
            record.round += 1;
            record.calls = calls.clone();
            put_request(&mut events, &record);
            ordinal = Some(next);
        }
        Transition::ToolStart { record } => {
            let conversation = parent(store, parent_head)?;
            canonical_shape(record, "tool")?;
            let request = require_request_running_or_cancelling(&conversation, &record.request)?;
            if record.status != CallStatus::Started || record.task.is_none() {
                return Err("tool.start requires started status and task".to_string());
            }
            if conversation
                .tool(&record.request, record.round, &record.id)?
                .is_some()
            {
                return Err(format!("tool {:?} already exists", record.id));
            }
            validate_current_call(&request, record)?;
            validate_tool_source_tree(&conversation, record)?;
            put_tool(&mut events, record);
        }
        Transition::ToolComplete {
            record,
            payloads,
            files,
        } => {
            let conversation = parent(store, parent_head)?;
            let request = require_request_running_or_cancelling(&conversation, &record.request)?;
            validate_tool_completion(
                &conversation,
                &request,
                record,
                payloads,
                files,
                &mut builder,
                &mut events,
            )?;
        }
        Transition::AsyncStart { record } => {
            let conversation = parent(store, parent_head)?;
            canonical_shape(record, "async record")?;
            if record.status != TaskStatus::Pending {
                return Err("async.start requires pending status".to_string());
            }
            if conversation.async_task(&record.task)?.is_some() {
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
            let conversation = parent(store, parent_head)?;
            let mut record = conversation
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
            let conversation = parent(store, parent_head)?;
            let request = require_request_running_or_cancelling(&conversation, &tool.request)?;
            canonical_shape(tool, "tool")?;
            canonical_shape(child, "child")?;
            if conversation
                .tool(&tool.request, tool.round, &tool.id)?
                .is_some()
            {
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
            let expected = ids::child_id(
                &conversation.identity()?.id,
                &tool.request,
                tool.round,
                &tool.id,
            )?;
            if child.id != expected {
                return Err(format!(
                    "subagent child id mismatch: got {:?}, expected {expected:?}",
                    child.id
                ));
            }
            if child.spawn_intent.request != tool.request
                || child.spawn_intent.round != tool.round
                || child.spawn_intent.tool != tool.id
            {
                return Err("subagent spawn intent does not match tool".to_string());
            }
            validate_current_call(&request, tool)?;
            if conversation.child(&child.id)?.is_some() {
                return Err(format!("subagent {:?} already exists", child.id));
            }
            validate_new_tool_payloads(&conversation, tool, payloads)?;
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
            let conversation = parent(store, parent_head)?;
            let mut record = require_child(&conversation, child)?;
            record.status = record.status.finish(*status)?;
            record.terminal_head = Some(terminal_head.clone());
            canonical_shape(&record, "child record")?;
            put_child(&mut events, &record);
        }
        Transition::PublicationPending { record } => {
            let conversation = parent(store, parent_head)?;
            canonical_shape(record, "publication")?;
            paths::validate_protocol_id_component(&record.id)?;
            if record.status != PublicationStatus::Pending {
                return Err("publication.pending requires pending status".to_string());
            }
            if conversation.publication(&record.id)?.is_some() {
                return Err(format!("publication {:?} already exists", record.id));
            }
            let projection = ids::projection_id(&record.descriptor.to_value())?;
            let expected = ids::publication_id(
                &conversation.identity()?.id,
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
            let conversation = parent(store, parent_head)?;
            let mut record = conversation
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
            let conversation = parent(store, parent_head)?;
            if files.is_empty() {
                return Err("files.apply requires at least one file".to_string());
            }
            apply_files(&conversation, &mut builder, files, "file path")?;
        }
    }

    Ok(Applied {
        tree: builder.build(store)?,
        ordinal,
        events,
    })
}

fn validate_fork_quiescence(conversation: &Conversation<'_>) -> Result<(), String> {
    if conversation.active_turn()?.is_some() {
        return Err("cannot fork a conversation with an active or cancelling request".to_string());
    }
    for request in conversation.turn_ids()? {
        let record = conversation
            .turn(&request)?
            .ok_or_else(|| format!("request {request} disappeared"))?;
        for round in 0..record.round {
            if conversation
                .tools(&request, round)?
                .into_iter()
                .any(|tool| !tool.is_terminal())
            {
                return Err("cannot fork a conversation with a started tool".to_string());
            }
        }
    }
    if conversation
        .async_tasks()?
        .into_iter()
        .any(|task| task.status == TaskStatus::Pending)
    {
        return Err("cannot fork a conversation with a nonterminal async task".to_string());
    }
    if conversation
        .publications()?
        .into_iter()
        .any(|publication| publication.status == PublicationStatus::Pending)
    {
        return Err("cannot fork a conversation with a nonterminal publication".to_string());
    }
    Ok(())
}

pub fn mint(
    store: &mut dyn ObjectStore,
    parent: &Oid,
    applied: &Applied,
    kind: Kind,
    signature: &Signature,
) -> Result<Oid, String> {
    if kind == Kind::ConversationRoot && parent != &super::oid::g3() {
        return Err("conversation root must parent G3".to_string());
    }
    if kind != Kind::ConversationRoot && parent == &super::oid::g3() {
        return Err("only a conversation root may parent G3".to_string());
    }
    store
        .write_commit(&CommitInfo {
            tree: applied.tree.clone(),
            parents: vec![parent.clone()],
            author: signature.clone(),
            committer: signature.clone(),
            extra_headers: Vec::new(),
            message: super::events::encode(kind, &applied.events),
        })
        .map_err(String::from)
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

fn parent<'s>(store: &'s dyn ObjectStore, tree: Option<&Oid>) -> Result<Conversation<'s>, String> {
    Conversation::open(store, tree.expect("non-root transition has parent commit"))
}

fn canonical_shape<T: Record + PartialEq>(record: &T, what: &str) -> Result<(), String> {
    if T::from_value(&record.to_value())? != *record {
        return Err(format!("{what} does not have a canonical record shape"));
    }
    Ok(())
}

fn put_request(events: &mut Vec<Event>, record: &TurnRecord) {
    events.push(Event::Request(record.clone()));
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
        entry.encode(),
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

fn validate_entry_resolution(
    conversation: &Conversation<'_>,
    entry: &TranscriptEntry,
    builder: &mut TreeBuilder,
) -> Result<(), String> {
    let proposal = entry
        .proposal
        .as_ref()
        .map(|proposal| {
            let source_tree = conversation
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

fn require_active(conversation: &Conversation<'_>, request: &Oid) -> Result<TurnRecord, String> {
    let active = conversation
        .active_turn()?
        .ok_or_else(|| "there is no active request".to_string())?;
    if active.id != *request {
        return Err(format!("active request is {}, not {request}", active.id));
    }
    Ok(active)
}

fn require_request_running_or_cancelling(
    conversation: &Conversation<'_>,
    request: &Oid,
) -> Result<TurnRecord, String> {
    let record = require_active(conversation, request)?;
    if !matches!(record.status, TurnStatus::Running | TurnStatus::Cancelling) {
        return Err("tool transition requires running or cancelling request".to_string());
    }
    Ok(record)
}

fn validate_tool_source_tree(
    conversation: &Conversation<'_>,
    record: &CallRecord,
) -> Result<(), String> {
    if record.source_tree_name.is_some() && record.input_commit.is_none() {
        return Err("a selected source tree requires an input commit".to_string());
    }
    if record.source_tree_name.is_none() {
        if let Some(input) = &record.input_commit {
            if conversation.commit() != Some(input) {
                return Err("tool input conversation is stale".into());
            }
        }
    }
    if let (Some(name), Some(input)) = (&record.source_tree_name, &record.input_commit) {
        let source_tree = conversation
            .source_tree(name)?
            .ok_or_else(|| format!("source tree {name:?} does not exist"))?;
        if source_tree.commit != *input {
            return Err("tool input_commit is stale".to_string());
        }
    }
    Ok(())
}

fn validate_current_call(request: &TurnRecord, record: &CallRecord) -> Result<(), String> {
    if record.round.checked_add(1) != Some(request.round)
        || !request.calls.iter().any(|call| call.id == record.id)
    {
        return Err("tool not declared by the current round".to_string());
    }
    Ok(())
}

#[allow(clippy::type_complexity)]
fn validate_tool_completion(
    conversation: &Conversation<'_>,
    request: &TurnRecord,
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
    let existing = conversation.tool(&record.request, record.round, &record.id)?;
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
        if conversation.task(task)?.is_none() {
            return Err("startless tool.complete must refer to an existing task".to_string());
        }
    }
    if existing.is_none() {
        validate_current_call(request, record)?;
        validate_tool_source_tree(conversation, record)?;
    }
    let result = record
        .result
        .as_ref()
        .ok_or_else(|| "tool.complete requires a result".to_string())?;
    if CallRecord::expected_status(result, record.source_tree_resolution.as_ref()) != record.status
    {
        return Err("tool status does not match result and resolution".to_string());
    }
    validate_new_tool_payloads(conversation, record, payloads)?;
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
    apply_files(conversation, builder, files, "tool file path")?;
    if let Some(pointer) = record
        .source_tree_resolution
        .as_ref()
        .and_then(SourceTreeResolution::new_pointer)
    {
        let name = record
            .source_tree_name
            .as_ref()
            .ok_or_else(|| "tool source tree resolution requires source_tree_name".to_string())?;
        let source_tree = conversation
            .source_tree(name)?
            .ok_or_else(|| format!("source tree {name:?} does not exist"))?;
        move_source_tree_pointer(
            builder,
            name,
            &source_tree.commit,
            Some(pointer),
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
    let dir = paths::call_payload_dir(record.request.as_str(), record.round, &record.id);
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
    conversation: &Conversation<'_>,
    record: &CallRecord,
    payloads: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let dir = paths::call_payload_dir(record.request.as_str(), record.round, &record.id);
    for (name, _) in payloads {
        paths::validate_component(name)?;
        let path = format!("{dir}/{name}");
        if conversation.snapshot().exists(&path)? {
            return Err(format!("tool payload {path:?} already exists"));
        }
    }
    Ok(())
}

#[allow(clippy::type_complexity)]
fn apply_files(
    conversation: &Conversation<'_>,
    builder: &mut TreeBuilder,
    files: &[(String, Option<(Mode, Vec<u8>)>)],
    unique_what: &str,
) -> Result<(), String> {
    let names: Vec<String> = files.iter().map(|(path, _)| path.clone()).collect();
    paths::validate_unique(&names, unique_what)?;
    for (relative, value) in files {
        paths::validate_source_tree_name(relative)?;
        let path = paths::files_path(relative);
        let before = conversation.snapshot().entry(&path)?;
        let changed = match (before, value) {
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
            (Some(entry), Some((mode, bytes))) => {
                entry.mode != *mode
                    || if matches!(mode, Mode::Commit | Mode::Tree) {
                        entry.oid.encode_line() != *bytes
                    } else {
                        conversation.snapshot().read(&path)?.as_deref() != Some(bytes.as_slice())
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

fn require_child(conversation: &Conversation<'_>, child: &str) -> Result<ChildRecord, String> {
    conversation
        .child(child)?
        .ok_or_else(|| format!("subagent {child:?} does not exist"))
}

fn validate_caos_path(value: &str, what: &str) -> Result<(), String> {
    paths::validate_tree_path(value)?;
    if !value.starts_with(".caos/") {
        return Err(format!("{what} must start with .caos/, got {value:?}"));
    }
    Ok(())
}
