use std::collections::{BTreeMap, BTreeSet};

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
        workspaces: BTreeMap<String, (Oid, Option<WorkspaceOrigin>)>,
        files_seed: Option<Oid>,
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
    RequestAdmit {
        record: RequestRecord,
    },
    RequestClaim {
        request: Oid,
        latest_message: String,
    },
    RequestInterject {
        request: Oid,
        entry: TranscriptEntry,
        payloads: Vec<(String, Vec<u8>)>,
    },
    RequestEscape {
        request: Oid,
        reason: Option<String>,
    },
    RequestTerminal {
        request: Oid,
        outcome: RequestOutcome,
    },
    ModelComplete {
        request: Oid,
        entry: TranscriptEntry,
        payloads: Vec<(String, Vec<u8>)>,
        calls: Vec<DeclaredCall>,
    },
    ToolStart {
        record: ToolRecord,
    },
    ToolComplete {
        record: ToolRecord,
        payloads: Vec<(String, Vec<u8>)>,
        files: Vec<(String, Option<(Mode, Vec<u8>)>)>,
    },
    AsyncStart {
        record: AsyncRecord,
    },
    AsyncTerminal {
        task: Oid,
        status: AsyncStatus,
        result: Option<Oid>,
        reason: Option<String>,
    },
    SubagentSpawn {
        tool: ToolRecord,
        payloads: Vec<(String, Vec<u8>)>,
        child: ChildRecord,
    },
    SubagentTerminal {
        child: String,
        terminal_head: Oid,
        status: ChildStatus,
        child_workspaces: BTreeMap<String, ChildWorkspace>,
    },
    SubagentApply {
        child: String,
        application: Application,
    },
    WorkspaceCreate {
        name: String,
        commit: Oid,
        origin: Option<WorkspaceOrigin>,
    },
    WorkspaceRollback {
        name: String,
        commit: Oid,
    },
    WorkspaceRemove {
        name: String,
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
    pub fn kind(&self) -> Kind {
        match self {
            Transition::ConversationRoot { .. } => Kind::ConversationRoot,
            Transition::ConversationFork { .. } => Kind::ConversationFork,
            Transition::TitleSet { .. } => Kind::MetadataTitleSet,
            Transition::MessageAppend { .. } => Kind::MessageAppend,
            Transition::RequestAdmit { .. } => Kind::RequestAdmit,
            Transition::RequestClaim { .. } => Kind::RequestClaim,
            Transition::RequestInterject { .. } => Kind::RequestInterject,
            Transition::RequestEscape { .. } => Kind::RequestEscape,
            Transition::RequestTerminal { .. } => Kind::RequestTerminal,
            Transition::ModelComplete { .. } => Kind::ModelComplete,
            Transition::ToolStart { .. } => Kind::ToolStart,
            Transition::ToolComplete { .. } => Kind::ToolComplete,
            Transition::AsyncStart { .. } => Kind::AsyncStart,
            Transition::AsyncTerminal { .. } => Kind::AsyncTerminal,
            Transition::SubagentSpawn { .. } => Kind::SubagentSpawn,
            Transition::SubagentTerminal { .. } => Kind::SubagentTerminal,
            Transition::SubagentApply { .. } => Kind::SubagentApply,
            Transition::WorkspaceCreate { .. } => Kind::WorkspaceCreate,
            Transition::WorkspaceRollback { .. } => Kind::WorkspaceRollback,
            Transition::WorkspaceRemove { .. } => Kind::WorkspaceRemove,
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
}

pub fn apply(
    store: &mut dyn ObjectStore,
    parent_tree: Option<&Oid>,
    transition: &Transition,
) -> Result<Applied, String> {
    if matches!(transition, Transition::ConversationRoot { .. }) {
        if parent_tree.is_some() {
            return Err("conversation root requires no parent tree".to_string());
        }
    } else if parent_tree.is_none() {
        return Err(format!(
            "{} requires a parent tree",
            transition.kind().as_str()
        ));
    }
    let mut builder = TreeBuilder::from(parent_tree.cloned());
    let mut ordinal = None;

    match transition {
        Transition::ConversationRoot {
            identity,
            title,
            workspaces,
            files_seed,
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
            builder.put(paths::IDENTITY, Mode::Blob, identity.encode());
            builder.put(paths::TITLE, Mode::Blob, encode_title(title)?);
            for (name, (commit, origin)) in workspaces {
                paths::validate_workspace_name(name)?;
                put_workspace(&mut builder, name, commit, commit, origin.as_ref());
            }
            if let Some(seed) = files_seed {
                store
                    .read_tree(seed)
                    .map_err(|error| format!("files seed {seed} must be a tree: {error}"))?;
                builder.put_oid(paths::FILES_DIR, Mode::Tree, seed.clone());
            }
        }
        Transition::ConversationFork { identity, title } => {
            let conversation = parent(store, parent_tree)?;
            canonical_shape(identity, "identity")?;
            if !matches!(identity.kind, IdentityKind::Fork { .. }) {
                return Err("conversation fork identity must have fork kind".to_string());
            }
            if identity.id == conversation.identity()?.id {
                return Err("conversation fork identity id must differ from its source".to_string());
            }
            validate_fork_quiescence(&conversation)?;
            let running: Vec<String> = conversation
                .children()?
                .into_iter()
                .filter(|child| child.status == ChildStatus::Running)
                .map(|child| child.id)
                .collect();
            builder.put(paths::IDENTITY, Mode::Blob, identity.encode());
            builder.put(paths::TITLE, Mode::Blob, encode_title(title)?);
            for child in running {
                builder.delete(&paths::subagent_record_path(&child));
            }
        }
        Transition::TitleSet { title } => {
            let conversation = parent(store, parent_tree)?;
            if conversation.title()? == *title {
                return Err("no-op".to_string());
            }
            builder.put(paths::TITLE, Mode::Blob, encode_title(title)?);
        }
        Transition::MessageAppend { entry, payloads } => {
            let conversation = parent(store, parent_tree)?;
            if !matches!(entry.role, Role::User | Role::System) {
                return Err("message.append requires a user or system entry".to_string());
            }
            let next = conversation.transcript_len()?;
            validate_entry_resolution(&conversation, entry, &mut builder)?;
            put_transcript(&mut builder, next, entry, payloads)?;
            ordinal = Some(next);
        }
        Transition::RequestAdmit { record } => {
            let conversation = parent(store, parent_tree)?;
            canonical_shape(record, "request")?;
            if conversation.active_request()?.is_some() {
                return Err("cannot admit a request while another request is active".to_string());
            }
            if conversation.request(&record.id)?.is_some() {
                return Err(format!("request {} already exists", record.id));
            }
            if record.status != RequestStatus::Queued
                || record.round != 0
                || !record.calls.is_empty()
                || !record.interjections.is_empty()
            {
                return Err(
                    "admitted request must be queued at round zero with no calls or interjections"
                        .to_string(),
                );
            }
            put_request(&mut builder, record);
            builder.put(paths::ACTIVE_REQUEST, Mode::Blob, record.id.encode_line());
        }
        Transition::RequestClaim {
            request,
            latest_message,
        } => {
            let conversation = parent(store, parent_tree)?;
            let mut record = require_active(&conversation, request)?;
            if record.status != RequestStatus::Queued {
                return Err("request claim requires queued status".to_string());
            }
            paths::validate_protocol_id_component(latest_message)?;
            record.status = RequestStatus::Running;
            record.latest_message = Some(latest_message.clone());
            put_request(&mut builder, &record);
        }
        Transition::RequestInterject {
            request,
            entry,
            payloads,
        } => {
            let conversation = parent(store, parent_tree)?;
            let mut record = require_active(&conversation, request)?;
            if !matches!(
                record.status,
                RequestStatus::Queued | RequestStatus::Running | RequestStatus::Cancelling
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
            if matches!(
                record.status,
                RequestStatus::Running | RequestStatus::Cancelling
            ) {
                record.latest_message = Some(entry.message_id.clone());
            }
            put_request(&mut builder, &record);
            ordinal = Some(next);
        }
        Transition::RequestEscape { request, reason } => {
            let conversation = parent(store, parent_tree)?;
            let mut record = require_active(&conversation, request)?;
            match record.status {
                RequestStatus::Queued => {
                    record.status = RequestStatus::Idle;
                    record.escape_reason = reason.clone();
                    record.outcome = Some(RequestOutcome::Idle {
                        result: None,
                        interrupted: true,
                    });
                    builder.delete(paths::ACTIVE_REQUEST);
                }
                RequestStatus::Running => {
                    record.status = RequestStatus::Cancelling;
                    record.escape_reason = reason.clone();
                }
                _ => return Err("request escape requires queued or running status".to_string()),
            }
            put_request(&mut builder, &record);
        }
        Transition::RequestTerminal { request, outcome } => {
            let conversation = parent(store, parent_tree)?;
            let mut record = require_active(&conversation, request)?;
            if !matches!(
                record.status,
                RequestStatus::Running | RequestStatus::Cancelling
            ) {
                return Err("request terminal requires running or cancelling status".to_string());
            }
            if record.status == RequestStatus::Cancelling && record.round != 0 {
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
                RequestOutcome::Idle {
                    result: Some(path), ..
                } => {
                    validate_caos_path(path, "request result path")?;
                    RequestStatus::Idle
                }
                RequestOutcome::Idle { .. } => RequestStatus::Idle,
                RequestOutcome::Failed { error } => {
                    validate_caos_path(error, "request error path")?;
                    record.escape_reason = None;
                    RequestStatus::Failed
                }
            };
            record.latest_message = None;
            record.outcome = Some(outcome.clone());
            put_request(&mut builder, &record);
            builder.delete(paths::ACTIVE_REQUEST);
        }
        Transition::ModelComplete {
            request,
            entry,
            payloads,
            calls,
        } => {
            let conversation = parent(store, parent_tree)?;
            let mut record = require_active(&conversation, request)?;
            if record.status != RequestStatus::Running {
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
            put_request(&mut builder, &record);
            ordinal = Some(next);
        }
        Transition::ToolStart { record } => {
            let conversation = parent(store, parent_tree)?;
            canonical_shape(record, "tool")?;
            let request = require_request_running_or_cancelling(&conversation, &record.request)?;
            if record.status != ToolStatus::Started || record.task.is_none() {
                return Err("tool.start requires started status and task".to_string());
            }
            if conversation
                .tool(&record.request, record.round, &record.id)?
                .is_some()
            {
                return Err(format!("tool {:?} already exists", record.id));
            }
            validate_current_call(&request, record)?;
            validate_tool_workspace(&conversation, record)?;
            put_tool(&mut builder, record);
        }
        Transition::ToolComplete {
            record,
            payloads,
            files,
        } => {
            let conversation = parent(store, parent_tree)?;
            let request = require_request_running_or_cancelling(&conversation, &record.request)?;
            validate_tool_completion(
                &conversation,
                &request,
                record,
                payloads,
                files,
                &mut builder,
            )?;
        }
        Transition::AsyncStart { record } => {
            let conversation = parent(store, parent_tree)?;
            canonical_shape(record, "async record")?;
            if record.status != AsyncStatus::Pending {
                return Err("async.start requires pending status".to_string());
            }
            if conversation.async_task(&record.task)?.is_some() {
                return Err(format!("async task {} already exists", record.task));
            }
            put_async(&mut builder, record);
        }
        Transition::AsyncTerminal {
            task,
            status,
            result,
            reason,
        } => {
            let conversation = parent(store, parent_tree)?;
            let mut record = conversation
                .async_task(task)?
                .ok_or_else(|| format!("async task {task} does not exist"))?;
            if record.status != AsyncStatus::Pending {
                return Err("async terminal requires pending status".to_string());
            }
            if *status == AsyncStatus::Pending {
                return Err("async terminal status must be terminal".to_string());
            }
            record.status = *status;
            record.result = result.clone();
            record.reason = reason.clone();
            canonical_shape(&record, "async record")?;
            put_async(&mut builder, &record);
        }
        Transition::SubagentSpawn {
            tool,
            payloads,
            child,
        } => {
            let conversation = parent(store, parent_tree)?;
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
            if tool.task.is_some()
                || tool.status != ToolStatus::Complete
                || tool.workspace_resolution.is_some()
                || !tool.files.is_empty()
                || tool.files_outcome.is_some()
            {
                return Err(
                    "subagent spawn requires a startless complete tool without a proposal"
                        .to_string(),
                );
            }
            if child.status != ChildStatus::Running {
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
            let payload_paths = put_tool_payloads(&mut builder, tool, payloads)?;
            if !payload_paths.contains(observation) {
                return Err(
                    "subagent tool observation is not supplied by this transition".to_string(),
                );
            }
            put_tool(&mut builder, tool);
            builder.put(
                &paths::subagent_record_path(&child.id),
                Mode::Blob,
                child.encode(),
            );
        }
        Transition::SubagentTerminal {
            child,
            terminal_head,
            status,
            child_workspaces,
        } => {
            let conversation = parent(store, parent_tree)?;
            let mut record = require_child(&conversation, child)?;
            if record.status != ChildStatus::Running {
                return Err("subagent terminal requires running status".to_string());
            }
            if *status == ChildStatus::Running {
                return Err("subagent terminal status must be terminal".to_string());
            }
            for name in child_workspaces.keys() {
                paths::validate_workspace_name(name)?;
            }
            record.status = *status;
            record.terminal_head = Some(terminal_head.clone());
            record.child_workspaces = Some(child_workspaces.clone());
            put_child(&mut builder, &record);
        }
        Transition::SubagentApply { child, application } => {
            let conversation = parent(store, parent_tree)?;
            Application::from_value(&application.to_value())?;
            let mut record = require_child(&conversation, child)?;
            if record.status == ChildStatus::Running {
                return Err("cannot apply a running subagent".to_string());
            }
            let workspace = conversation
                .workspace(&application.parent_workspace_name)?
                .ok_or_else(|| {
                    format!(
                        "workspace {:?} does not exist",
                        application.parent_workspace_name
                    )
                })?;
            if application.parent_workspace.as_ref() != Some(&workspace.commit) {
                return Err("subagent application parent workspace is stale".to_string());
            }
            move_workspace_pointer(
                &mut builder,
                &application.parent_workspace_name,
                &workspace.commit,
                application.workspace_resolution.new_pointer(),
                "subagent application workspace update",
            )?;
            record.applications.push(application.clone());
            put_child(&mut builder, &record);
        }
        Transition::WorkspaceCreate {
            name,
            commit,
            origin,
        } => {
            let conversation = parent(store, parent_tree)?;
            paths::validate_workspace_name(name)?;
            if conversation.workspace(name)?.is_some() {
                return Err(format!("workspace {name:?} already exists"));
            }
            put_workspace(&mut builder, name, commit, commit, origin.as_ref());
        }
        Transition::WorkspaceRollback { name, commit } => {
            let conversation = parent(store, parent_tree)?;
            let workspace = conversation
                .workspace(name)?
                .ok_or_else(|| format!("workspace {name:?} does not exist"))?;
            move_workspace_pointer(
                &mut builder,
                name,
                &workspace.commit,
                Some(commit),
                "workspace rollback",
            )?;
        }
        Transition::WorkspaceRemove { name } => {
            let conversation = parent(store, parent_tree)?;
            if conversation.workspace(name)?.is_none() {
                return Err(format!("workspace {name:?} does not exist"));
            }
            builder.delete(&paths::workspace_dir(name));
        }
        Transition::PublicationPending { record } => {
            let conversation = parent(store, parent_tree)?;
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
            put_publication(&mut builder, record);
        }
        Transition::PublicationTerminal {
            publication,
            status,
            evidence,
            observed,
        } => {
            let conversation = parent(store, parent_tree)?;
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
            put_publication(&mut builder, &record);
        }
        Transition::FilesApply { files } => {
            let conversation = parent(store, parent_tree)?;
            if files.is_empty() {
                return Err("files.apply requires at least one file".to_string());
            }
            apply_files(&conversation, &mut builder, files, "file path")?;
        }
    }

    Ok(Applied {
        tree: builder.build(store)?,
        ordinal,
    })
}

fn validate_fork_quiescence(conversation: &Conversation<'_>) -> Result<(), String> {
    if conversation.active_request()?.is_some() {
        return Err("cannot fork a conversation with an active or cancelling request".to_string());
    }
    for request in conversation.request_ids()? {
        let record = conversation
            .request(&request)?
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
        .any(|task| task.status == AsyncStatus::Pending)
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
    tree: &Oid,
    kind: Kind,
    signature: &Signature,
) -> Result<Oid, String> {
    if kind == Kind::ConversationRoot && parent != &super::oid::g3() {
        return Err("conversation root must parent G3".to_string());
    }
    if kind != Kind::ConversationRoot && parent == &super::oid::g3() {
        return Err("only a conversation root may parent G3".to_string());
    }
    if kind == Kind::ConversationFork {
        let identity = Conversation::open_tree(store, tree)?.identity()?;
        let source = match identity.kind {
            IdentityKind::Fork { source } => source,
            IdentityKind::Root => {
                return Err("conversation fork identity must have fork kind".to_string())
            }
        };
        if source != *parent {
            return Err("conversation fork identity source must equal its parent".to_string());
        }
    }
    store
        .write_commit(&CommitInfo {
            tree: tree.clone(),
            parents: vec![parent.clone()],
            author: signature.clone(),
            committer: signature.clone(),
            extra_headers: Vec::new(),
            message: kind.message(),
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
    Conversation::open_tree(store, tree.expect("non-root transition has parent tree"))
}

fn canonical_shape<T: Record + PartialEq>(record: &T, what: &str) -> Result<(), String> {
    if T::from_value(&record.to_value())? != *record {
        return Err(format!("{what} does not have a canonical record shape"));
    }
    Ok(())
}

fn put_workspace(
    builder: &mut TreeBuilder,
    name: &str,
    commit: &Oid,
    initial: &Oid,
    origin: Option<&WorkspaceOrigin>,
) {
    builder.put(
        &paths::workspace_commit_path(name),
        Mode::Blob,
        commit.encode_line(),
    );
    builder.put(
        &paths::workspace_initial_path(name),
        Mode::Blob,
        initial.encode_line(),
    );
    if let Some(origin) = origin {
        builder.put(
            &paths::workspace_origin_path(name),
            Mode::Blob,
            origin.encode(),
        );
    }
}

fn put_request(builder: &mut TreeBuilder, record: &RequestRecord) {
    builder.put(
        &paths::request_record_path(record.id.as_str()),
        Mode::Blob,
        record.encode(),
    );
}

fn put_tool(builder: &mut TreeBuilder, record: &ToolRecord) {
    builder.put(
        &paths::tool_record_path(record.request.as_str(), record.round, &record.id),
        Mode::Blob,
        record.encode(),
    );
}

fn put_async(builder: &mut TreeBuilder, record: &AsyncRecord) {
    builder.put(
        &paths::async_record_path(record.task.as_str()),
        Mode::Blob,
        record.encode(),
    );
}

fn put_child(builder: &mut TreeBuilder, record: &ChildRecord) {
    builder.put(
        &paths::subagent_record_path(&record.id),
        Mode::Blob,
        record.encode(),
    );
}

fn put_publication(builder: &mut TreeBuilder, record: &PublicationRecord) {
    builder.put(
        &paths::publication_record_path(&record.id),
        Mode::Blob,
        record.encode(),
    );
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
            let workspace = conversation
                .workspace(&proposal.workspace_name)?
                .ok_or_else(|| format!("workspace {:?} does not exist", proposal.workspace_name))?;
            Ok::<(&Proposal, WorkspaceRecord), String>((proposal, workspace))
        })
        .transpose()?;
    let Some(resolution) = &entry.workspace_resolution else {
        return Ok(());
    };
    let (proposal, workspace) =
        proposal.ok_or_else(|| "workspace resolution requires a proposal".to_string())?;
    move_workspace_pointer(
        builder,
        &proposal.workspace_name,
        &workspace.commit,
        resolution.new_pointer(),
        "workspace pointer update",
    )?;
    Ok(())
}

fn require_active(conversation: &Conversation<'_>, request: &Oid) -> Result<RequestRecord, String> {
    let active = conversation
        .active_request()?
        .ok_or_else(|| "there is no active request".to_string())?;
    if active.id != *request {
        return Err(format!("active request is {}, not {request}", active.id));
    }
    Ok(active)
}

fn require_request_running_or_cancelling(
    conversation: &Conversation<'_>,
    request: &Oid,
) -> Result<RequestRecord, String> {
    let record = require_active(conversation, request)?;
    if !matches!(
        record.status,
        RequestStatus::Running | RequestStatus::Cancelling
    ) {
        return Err("tool transition requires running or cancelling request".to_string());
    }
    Ok(record)
}

fn validate_tool_workspace(
    conversation: &Conversation<'_>,
    record: &ToolRecord,
) -> Result<(), String> {
    if record.workspace_name.is_some() != record.input_workspace.is_some() {
        return Err(
            "tool workspace_name and input_workspace must both be absent or present".to_string(),
        );
    }
    if let (Some(name), Some(input)) = (&record.workspace_name, &record.input_workspace) {
        let workspace = conversation
            .workspace(name)?
            .ok_or_else(|| format!("workspace {name:?} does not exist"))?;
        if workspace.commit != *input {
            return Err("tool input_workspace is stale".to_string());
        }
    }
    Ok(())
}

fn validate_current_call(request: &RequestRecord, record: &ToolRecord) -> Result<(), String> {
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
    request: &RequestRecord,
    record: &ToolRecord,
    payloads: &[(String, Vec<u8>)],
    files: &[(String, Option<(Mode, Vec<u8>)>)],
    builder: &mut TreeBuilder,
) -> Result<(), String> {
    canonical_shape(record, "tool")?;
    if record.status == ToolStatus::Started {
        return Err("tool.complete requires a terminal status".to_string());
    }
    let existing = conversation.tool(&record.request, record.round, &record.id)?;
    if let Some(started) = &existing {
        if started.status != ToolStatus::Started {
            return Err("tool.complete requires an absent or started tool record".to_string());
        }
        if started.name != record.name
            || started.declaration_message != record.declaration_message
            || started.workspace_name != record.workspace_name
            || started.input_workspace != record.input_workspace
            || started.task != record.task
        {
            return Err("tool.complete identity fields do not match tool.start".to_string());
        }
    } else if record.task.is_some() {
        return Err("startless tool.complete must not carry a task".to_string());
    }
    if existing.is_none() {
        validate_current_call(request, record)?;
        validate_tool_workspace(conversation, record)?;
    }
    let result = record
        .result
        .as_ref()
        .ok_or_else(|| "tool.complete requires a result".to_string())?;
    if ToolRecord::expected_status(result, record.workspace_resolution.as_ref()) != record.status {
        return Err("tool status does not match result and resolution".to_string());
    }
    validate_new_tool_payloads(conversation, record, payloads)?;
    let payload_paths = put_tool_payloads(builder, record, payloads)?;
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
    if let Some(resolution) = &record.workspace_resolution {
        let name = record
            .workspace_name
            .as_ref()
            .ok_or_else(|| "tool workspace resolution requires workspace_name".to_string())?;
        let workspace = conversation
            .workspace(name)?
            .ok_or_else(|| format!("workspace {name:?} does not exist"))?;
        move_workspace_pointer(
            builder,
            name,
            &workspace.commit,
            resolution.new_pointer(),
            "tool workspace pointer update",
        )?;
    }
    put_tool(builder, record);
    Ok(())
}

fn move_workspace_pointer(
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
    builder.put(
        &paths::workspace_commit_path(name),
        Mode::Blob,
        pointer.encode_line(),
    );
    Ok(())
}

fn put_tool_payloads(
    builder: &mut TreeBuilder,
    record: &ToolRecord,
    payloads: &[(String, Vec<u8>)],
) -> Result<BTreeSet<String>, String> {
    put_payloads(
        builder,
        &paths::tool_payload_dir(record.request.as_str(), record.round, &record.id),
        payloads,
    )
}

fn validate_new_tool_payloads(
    conversation: &Conversation<'_>,
    record: &ToolRecord,
    payloads: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let dir = paths::tool_payload_dir(record.request.as_str(), record.round, &record.id);
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
        paths::validate_tree_path(relative)?;
        let path = paths::files_path(relative);
        let before = conversation.snapshot().entry(&path)?;
        let changed = match (before, value) {
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
            (Some(entry), Some((mode, bytes))) => {
                entry.mode != *mode
                    || conversation.snapshot().read(&path)?.as_deref() != Some(bytes.as_slice())
            }
        };
        if !changed {
            return Err(format!("file {relative:?} does not change the parent tree"));
        }
        match value {
            Some((mode @ (Mode::Blob | Mode::Executable), bytes)) => {
                builder.put(&path, *mode, bytes.clone());
            }
            Some((Mode::Tree, _)) => {
                return Err(format!("file {relative:?} cannot use tree mode"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v3::oid::ensure_genesis;
    use crate::v3::tree::{diff, MemoryStore};

    fn oid(character: char) -> Oid {
        Oid::parse(&character.to_string().repeat(40), "test oid").unwrap()
    }

    fn signature() -> Signature {
        client_signature("Tester", "tester@example.com", 1_700_000_000)
    }

    fn commit_transition(
        store: &mut MemoryStore,
        parent: &Oid,
        transition: Transition,
    ) -> (Oid, Applied) {
        let parent_tree = store.read_commit(parent).unwrap().tree;
        let applied = apply(store, Some(&parent_tree), &transition).unwrap();
        let head = mint(
            store,
            parent,
            &applied.tree,
            transition.kind(),
            &signature(),
        )
        .unwrap();
        (head, applied)
    }

    fn root(store: &mut MemoryStore) -> Oid {
        let genesis = ensure_genesis(store).unwrap();
        let transition = Transition::ConversationRoot {
            identity: Identity {
                id: "conversation-1".to_string(),
                kind: IdentityKind::Root,
                owner: None,
            },
            title: "Conversation".to_string(),
            workspaces: BTreeMap::from([("main".to_string(), (oid('a'), None))]),
            files_seed: None,
        };
        let applied = apply(store, None, &transition).unwrap();
        mint(
            store,
            &genesis,
            &applied.tree,
            transition.kind(),
            &signature(),
        )
        .unwrap()
    }

    fn request() -> RequestRecord {
        RequestRecord {
            id: oid('1'),
            request_head: oid('2'),
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

    #[allow(clippy::type_complexity)]
    fn assistant_entry() -> (TranscriptEntry, Vec<(String, Vec<u8>)>, Vec<DeclaredCall>) {
        let message_id = "assistant-1";
        let arguments = format!("{}/arguments", paths::transcript_payload_dir(1, message_id));
        (
            TranscriptEntry {
                message_id: message_id.to_string(),
                conversation: "conversation-1".to_string(),
                role: Role::Assistant,
                actor: "model".to_string(),
                request: Some(oid('1')),
                round: Some(0),
                model: Some("model".to_string()),
                blocks: vec![Block::ToolUse {
                    id: "tool-1".to_string(),
                    name: "edit".to_string(),
                    arguments,
                }],
                proposal: None,
                workspace_resolution: None,
            },
            vec![("arguments".to_string(), b"{}".to_vec())],
            vec![DeclaredCall {
                id: "tool-1".to_string(),
                name: "edit".to_string(),
            }],
        )
    }

    fn started_tool() -> ToolRecord {
        ToolRecord {
            request: oid('1'),
            round: 0,
            id: "tool-1".to_string(),
            name: "edit".to_string(),
            declaration_message: "assistant-1".to_string(),
            workspace_name: Some("main".to_string()),
            input_workspace: Some(oid('a')),
            status: ToolStatus::Started,
            task: Some(oid('3')),
            result: None,
            workspace_resolution: None,
            files: Vec::new(),
            files_outcome: None,
        }
    }

    fn completed_tool() -> (ToolRecord, Vec<(String, Vec<u8>)>) {
        let observation = format!(
            "{}/observation",
            paths::tool_payload_dir(oid('1').as_str(), 0, "tool-1")
        );
        (
            ToolRecord {
                status: ToolStatus::Complete,
                result: Some(ToolResult::Complete {
                    observation,
                    proposal: Some(oid('b')),
                }),
                workspace_resolution: Some(WorkspaceResolution::Direct {
                    current: oid('a'),
                    output: oid('b'),
                }),
                files: vec!["note.txt".to_string()],
                files_outcome: Some(FilesOutcome {
                    applied: vec!["note.txt".to_string()],
                    conflicted: Vec::new(),
                }),
                ..started_tool()
            },
            vec![("observation".to_string(), b"done".to_vec())],
        )
    }

    fn child_record(id: &str, status: ChildStatus) -> ChildRecord {
        let terminal = status != ChildStatus::Running;
        ChildRecord {
            id: id.to_string(),
            initial_head: oid('2'),
            initial_workspace: Some(oid('a')),
            request: oid('1'),
            relay: oid('4'),
            spawn_intent: SpawnIntent {
                request: oid('1'),
                round: 0,
                tool: "spawn".to_string(),
                workspace_name: None,
                input_workspace: None,
                prompt: ".caos/tools/prompt".to_string(),
                model: "model".to_string(),
                configuration: "configuration-hash".to_string(),
                files_seed: None,
            },
            status,
            applications: Vec::new(),
            terminal_head: terminal.then(|| oid('5')),
            child_workspaces: terminal.then(BTreeMap::new),
        }
    }

    fn assert_changed(store: &MemoryStore, before: Option<&Oid>, after: &Oid, expected: &[&str]) {
        let paths: Vec<String> = diff(store, before, after)
            .unwrap()
            .into_iter()
            .map(|change| change.path)
            .collect();
        assert_eq!(paths, expected);
    }

    #[test]
    fn complete_conversation_applies_exact_changes() {
        const REQUEST_1: &str = ".caos/requests/1111111111111111111111111111111111111111.json";
        const REQUEST_7: &str = ".caos/requests/7777777777777777777777777777777777777777.json";
        const REQUEST_8: &str = ".caos/requests/8888888888888888888888888888888888888888.json";
        const ACTIVE: &str = ".caos/requests/active";
        const WORKSPACE_MAIN: &str = ".caos/workspaces/main/commit";
        const CHILD: &str =
            ".caos/subagents/subagent-e3b31a607694c2667a4116c48e822fae70329acef9b44f61803978fd175cf537.json";
        const ASYNC: &str = ".caos/async/4444444444444444444444444444444444444444.json";
        const PUBLICATION: &str =
            ".caos/publications/f8c78ee8136e6bee1d121b22734b5f3210b113005643fac6c9d30e8e8863d61f.json";
        const USER_BODY: &str = ".caos/transcript/000000000/000000000000-user-0/body";
        const BASH_OBSERVATION: &str =
            ".caos/tools/1111111111111111111111111111111111111111/0000/bash-call/observation";

        const SPINE: &[(Kind, &[&str])] = &[
            (
                Kind::ConversationRoot,
                &[
                    ".caos/format",
                    ".caos/identity.json",
                    ".caos/title",
                    WORKSPACE_MAIN,
                    ".caos/workspaces/main/initial",
                    "files/notes.md",
                ],
            ),
            (Kind::MetadataTitleSet, &[".caos/title"]),
            (
                Kind::MessageAppend,
                &[
                    ".caos/transcript/000000000/000000000000-user-0.json",
                    USER_BODY,
                ],
            ),
            (Kind::RequestAdmit, &[REQUEST_1, ACTIVE]),
            (Kind::RequestClaim, &[REQUEST_1]),
            (
                Kind::RequestInterject,
                &[
                    REQUEST_1,
                    ".caos/transcript/000000000/000000000001-interjection-1.json",
                    WORKSPACE_MAIN,
                ],
            ),
            (
                Kind::ModelComplete,
                &[
                    REQUEST_1,
                    ".caos/transcript/000000000/000000000002-assistant-1.json",
                    ".caos/transcript/000000000/000000000002-assistant-1/bash-call-arguments",
                    ".caos/transcript/000000000/000000000002-assistant-1/read-call-arguments",
                ],
            ),
            (
                Kind::ToolComplete,
                &[
                    ".caos/tools/1111111111111111111111111111111111111111/0000/read-call.json",
                    ".caos/tools/1111111111111111111111111111111111111111/0000/read-call/observation",
                ],
            ),
            (
                Kind::ToolStart,
                &[".caos/tools/1111111111111111111111111111111111111111/0000/bash-call.json"],
            ),
            (
                Kind::ToolComplete,
                &[
                    ".caos/tools/1111111111111111111111111111111111111111/0000/bash-call.json",
                    BASH_OBSERVATION,
                    WORKSPACE_MAIN,
                    "files/tool.txt",
                ],
            ),
            (
                Kind::ModelComplete,
                &[
                    REQUEST_1,
                    ".caos/transcript/000000000/000000000003-assistant-2.json",
                    ".caos/transcript/000000000/000000000003-assistant-2/async-call-arguments",
                    ".caos/transcript/000000000/000000000003-assistant-2/spawn-call-arguments",
                ],
            ),
            (
                Kind::SubagentSpawn,
                &[
                    CHILD,
                    ".caos/tools/1111111111111111111111111111111111111111/0001/spawn-call.json",
                    ".caos/tools/1111111111111111111111111111111111111111/0001/spawn-call/observation",
                ],
            ),
            (Kind::AsyncStart, &[ASYNC]),
            (
                Kind::RequestInterject,
                &[
                    REQUEST_1,
                    ".caos/transcript/000000000/000000000004-interjection-3.json",
                ],
            ),
            (Kind::AsyncTerminal, &[ASYNC]),
            (Kind::SubagentTerminal, &[CHILD]),
            (Kind::SubagentApply, &[CHILD, WORKSPACE_MAIN]),
            (
                Kind::ModelComplete,
                &[
                    REQUEST_1,
                    ".caos/transcript/000000000/000000000005-assistant-4.json",
                ],
            ),
            (Kind::RequestTerminal, &[REQUEST_1, ACTIVE]),
            (
                Kind::WorkspaceCreate,
                &[
                    ".caos/workspaces/side/commit",
                    ".caos/workspaces/side/initial",
                    ".caos/workspaces/side/origin",
                ],
            ),
            (Kind::WorkspaceRollback, &[WORKSPACE_MAIN]),
            (
                Kind::WorkspaceRemove,
                &[
                    ".caos/workspaces/side/commit",
                    ".caos/workspaces/side/initial",
                    ".caos/workspaces/side/origin",
                ],
            ),
            (Kind::PublicationPending, &[PUBLICATION]),
            (Kind::PublicationTerminal, &[PUBLICATION]),
            (Kind::FilesApply, &["files/final.txt"]),
            (Kind::RequestAdmit, &[REQUEST_7, ACTIVE]),
            (Kind::RequestEscape, &[REQUEST_7, ACTIVE]),
            (Kind::RequestAdmit, &[REQUEST_8, ACTIVE]),
            (Kind::RequestClaim, &[REQUEST_8]),
            (Kind::RequestEscape, &[REQUEST_8]),
            (Kind::RequestTerminal, &[REQUEST_8, ACTIVE]),
            (
                Kind::ConversationFork,
                &[".caos/identity.json", ".caos/title"],
            ),
        ];

        let mut store = MemoryStore::new();
        let (head, applied) = crate::v3::fixtures::golden_with_applied(&mut store);
        let mut commits = Vec::new();
        let mut current = head.clone();
        while current != super::super::oid::g3() {
            let info = store.read_commit(&current).unwrap();
            commits.push((current, info.clone()));
            current = info.parents[0].clone();
        }
        commits.reverse();
        assert_eq!(commits.len(), SPINE.len());
        for ((_, info), (expected_kind, expected_paths)) in commits.iter().zip(SPINE) {
            assert_eq!(Kind::parse_message(&info.message).unwrap(), *expected_kind);
            let parent = &info.parents[0];
            let parent_tree = (parent != &super::super::oid::g3())
                .then(|| store.read_commit(parent).unwrap().tree);
            assert_changed(&store, parent_tree.as_ref(), &info.tree, expected_paths);
        }

        assert_eq!(applied[0].ordinal, Some(0));
        assert_eq!(applied[1].ordinal, Some(2));
        let view = Conversation::open(&store, &head).unwrap();
        assert_eq!(view.identity().unwrap().id, "golden-fork");
        assert_eq!(view.title().unwrap(), "Golden Fork");
        assert_eq!(view.workspace("main").unwrap().unwrap().commit, oid('f'));
        assert_eq!(view.transcript_len().unwrap(), 6);
        assert_eq!(view.payload(USER_BODY).unwrap(), b"Build it");
        assert_eq!(view.payload(BASH_OBSERVATION).unwrap(), b"changed");
        assert_eq!(view.file("notes.md").unwrap().unwrap(), b"seeded notes\n");
        assert_eq!(view.file("tool.txt").unwrap().unwrap(), b"tool output\n");
        assert_eq!(view.file("final.txt").unwrap().unwrap(), b"final\n");
        assert!(view.active_request().unwrap().is_none());
        assert_eq!(
            view.request(&oid('1')).unwrap().unwrap().status,
            RequestStatus::Idle
        );
        assert_eq!(
            view.request_ids().unwrap(),
            vec![oid('1'), oid('7'), oid('8')]
        );
        assert_eq!(view.tools(&oid('1'), 0).unwrap().len(), 2);
    }

    #[test]
    fn mint_and_signatures_are_exact() {
        let mut store = MemoryStore::new();
        let parent = root(&mut store);
        let tree = store.read_commit(&parent).unwrap().tree;
        let signature = signature();
        let commit = mint(&mut store, &parent, &tree, Kind::FilesApply, &signature).unwrap();
        let info = store.read_commit(&commit).unwrap();
        assert_eq!(info.parents, vec![parent]);
        assert_eq!(info.message, b"files.apply\n");
        assert_eq!(inherited_signature(&store, &commit), Ok(signature));
    }

    #[test]
    fn fork_rewrites_metadata_and_removes_only_running_children() {
        let mut store = MemoryStore::new();
        let head = root(&mut store);
        let root_tree = store.read_commit(&head).unwrap().tree;
        let running = child_record("running-child", ChildStatus::Running);
        let also_running = child_record("also-running-child", ChildStatus::Running);
        let completed = child_record("completed-child", ChildStatus::Completed);
        let mut builder = TreeBuilder::from(Some(root_tree));
        builder.put(
            &paths::subagent_record_path(&running.id),
            Mode::Blob,
            running.encode(),
        );
        builder.put(
            &paths::subagent_record_path(&also_running.id),
            Mode::Blob,
            also_running.encode(),
        );
        builder.put(
            &paths::subagent_record_path(&completed.id),
            Mode::Blob,
            completed.encode(),
        );
        let source_tree = builder.build(&mut store).unwrap();
        let transition = Transition::ConversationFork {
            identity: Identity {
                id: "fork".to_string(),
                kind: IdentityKind::Fork {
                    source: head.clone(),
                },
                owner: None,
            },
            title: "Fork".to_string(),
        };
        let applied = apply(&mut store, Some(&source_tree), &transition).unwrap();
        assert_changed(
            &store,
            Some(&source_tree),
            &applied.tree,
            &[
                ".caos/identity.json",
                ".caos/subagents/also-running-child.json",
                ".caos/subagents/running-child.json",
                ".caos/title",
            ],
        );
        let view = Conversation::open_tree(&store, &applied.tree).unwrap();
        assert_eq!(view.identity().unwrap().id, "fork");
        assert_eq!(view.title().unwrap(), "Fork");
        assert!(view.child("running-child").unwrap().is_none());
        assert!(view.child("also-running-child").unwrap().is_none());
        assert_eq!(
            view.child("completed-child").unwrap().unwrap().status,
            ChildStatus::Completed
        );
    }

    fn fork_error(store: &mut MemoryStore, source: &Oid, tree: &Oid) -> String {
        apply(
            store,
            Some(tree),
            &Transition::ConversationFork {
                identity: Identity {
                    id: "fork".to_string(),
                    kind: IdentityKind::Fork {
                        source: source.clone(),
                    },
                    owner: None,
                },
                title: "Fork".to_string(),
            },
        )
        .unwrap_err()
    }

    #[test]
    fn fork_rejects_active_or_cancelling_request() {
        let mut store = MemoryStore::new();
        let head = root(&mut store);
        let (head, applied) = commit_transition(
            &mut store,
            &head,
            Transition::RequestAdmit { record: request() },
        );
        assert_eq!(
            fork_error(&mut store, &head, &applied.tree),
            "cannot fork a conversation with an active or cancelling request"
        );
    }

    #[test]
    fn fork_rejects_started_tool() {
        let mut store = MemoryStore::new();
        let head = root(&mut store);
        let root_tree = store.read_commit(&head).unwrap().tree;
        let mut terminal = request();
        terminal.round = 1;
        terminal.status = RequestStatus::Idle;
        terminal.outcome = Some(RequestOutcome::Idle {
            result: None,
            interrupted: false,
        });
        let mut builder = TreeBuilder::from(Some(root_tree));
        builder.put(
            &paths::request_record_path(terminal.id.as_str()),
            Mode::Blob,
            terminal.encode(),
        );
        let tool = started_tool();
        builder.put(
            &paths::tool_record_path(tool.request.as_str(), tool.round, &tool.id),
            Mode::Blob,
            tool.encode(),
        );
        let tree = builder.build(&mut store).unwrap();
        assert_eq!(
            fork_error(&mut store, &head, &tree),
            "cannot fork a conversation with a started tool"
        );
    }

    #[test]
    fn fork_rejects_nonterminal_async_task() {
        let mut store = MemoryStore::new();
        let head = root(&mut store);
        let root_tree = store.read_commit(&head).unwrap().tree;
        let task = oid('4');
        let mut builder = TreeBuilder::from(Some(root_tree));
        builder.put(
            &paths::async_record_path(task.as_str()),
            Mode::Blob,
            AsyncRecord {
                task,
                status: AsyncStatus::Pending,
                target_ref: Some("refs/heads/main".to_string()),
                result: None,
                reason: None,
            }
            .encode(),
        );
        let tree = builder.build(&mut store).unwrap();
        assert_eq!(
            fork_error(&mut store, &head, &tree),
            "cannot fork a conversation with a nonterminal async task"
        );
    }

    #[test]
    fn fork_rejects_nonterminal_publication() {
        let mut store = MemoryStore::new();
        let head = root(&mut store);
        let root_tree = store.read_commit(&head).unwrap().tree;
        let publication = PublicationRecord {
            id: "publication-1".to_string(),
            key: "key".to_string(),
            descriptor: Descriptor {
                source_base: oid('a'),
                source_head: oid('b'),
                target_base: oid('a'),
                policy: "squash".to_string(),
                implementation: "project-v1".to_string(),
                commit_policy: "single".to_string(),
            },
            planned_head: oid('b'),
            repository: "repo".to_string(),
            refname: "refs/heads/main".to_string(),
            expected_old: Some(oid('a')),
            workspace_name: "main".to_string(),
            status: PublicationStatus::Pending,
            evidence: None,
            observed: None,
        };
        let mut builder = TreeBuilder::from(Some(root_tree));
        builder.put(
            &paths::publication_record_path(&publication.id),
            Mode::Blob,
            publication.encode(),
        );
        let tree = builder.build(&mut store).unwrap();
        assert_eq!(
            fork_error(&mut store, &head, &tree),
            "cannot fork a conversation with a nonterminal publication"
        );
    }

    #[test]
    fn core_preconditions_reject_invalid_transitions() {
        let mut store = MemoryStore::new();
        let root_head = root(&mut store);
        let root_tree = store.read_commit(&root_head).unwrap().tree;
        assert!(apply(
            &mut store,
            Some(&root_tree),
            &Transition::TitleSet {
                title: "Conversation".to_string()
            }
        )
        .unwrap_err()
        .contains("no-op"));
        assert!(apply(
            &mut store,
            Some(&root_tree),
            &Transition::WorkspaceCreate {
                name: "main".to_string(),
                commit: oid('b'),
                origin: None,
            }
        )
        .is_err());
        assert!(apply(
            &mut store,
            Some(&root_tree),
            &Transition::WorkspaceRollback {
                name: "main".to_string(),
                commit: oid('a'),
            }
        )
        .is_err());
        assert!(apply(
            &mut store,
            Some(&root_tree),
            &Transition::FilesApply { files: Vec::new() }
        )
        .is_err());

        let (queued_head, queued) = commit_transition(
            &mut store,
            &root_head,
            Transition::RequestAdmit { record: request() },
        );
        assert!(apply(
            &mut store,
            Some(&queued.tree),
            &Transition::RequestAdmit { record: request() }
        )
        .is_err());
        assert!(apply(
            &mut store,
            Some(&queued.tree),
            &Transition::ConversationFork {
                identity: Identity {
                    id: "fork".to_string(),
                    kind: IdentityKind::Fork {
                        source: queued_head.clone()
                    },
                    owner: None,
                },
                title: "Fork".to_string(),
            }
        )
        .is_err());

        let (running_head, running) = commit_transition(
            &mut store,
            &queued_head,
            Transition::RequestClaim {
                request: oid('1'),
                latest_message: "message".to_string(),
            },
        );
        assert!(apply(
            &mut store,
            Some(&running.tree),
            &Transition::RequestClaim {
                request: oid('1'),
                latest_message: "message".to_string(),
            }
        )
        .is_err());
        let (mut wrong_entry, payloads, calls) = assistant_entry();
        wrong_entry.round = Some(4);
        assert!(apply(
            &mut store,
            Some(&running.tree),
            &Transition::ModelComplete {
                request: oid('1'),
                entry: wrong_entry,
                payloads,
                calls,
            }
        )
        .is_err());
        let (mut entry, payloads, calls) = assistant_entry();
        if let Block::ToolUse { arguments, .. } = &mut entry.blocks[0] {
            *arguments = format!(
                "{}/arguments",
                paths::transcript_payload_dir(0, "assistant-1")
            );
        }
        let (model_head, model) = commit_transition(
            &mut store,
            &running_head,
            Transition::ModelComplete {
                request: oid('1'),
                entry,
                payloads,
                calls,
            },
        );
        let mut stale = started_tool();
        stale.input_workspace = Some(oid('b'));
        assert!(apply(
            &mut store,
            Some(&model.tree),
            &Transition::ToolStart { record: stale }
        )
        .is_err());
        let (_, started) = commit_transition(
            &mut store,
            &model_head,
            Transition::ToolStart {
                record: started_tool(),
            },
        );
        let (record, payloads) = completed_tool();
        assert!(apply(
            &mut store,
            Some(&started.tree),
            &Transition::ToolComplete {
                record,
                payloads,
                files: Vec::new(),
            }
        )
        .unwrap_err()
        .contains("files do not match"));

        let bad_child = ChildRecord {
            id: "wrong-child".to_string(),
            initial_head: running_head.clone(),
            initial_workspace: Some(oid('a')),
            request: oid('1'),
            relay: oid('4'),
            spawn_intent: SpawnIntent {
                request: oid('1'),
                round: 0,
                tool: "spawn".to_string(),
                workspace_name: None,
                input_workspace: None,
                prompt: ".caos/tools/prompt".to_string(),
                model: "model".to_string(),
                configuration: "configuration-hash".to_string(),
                files_seed: None,
            },
            status: ChildStatus::Running,
            applications: Vec::new(),
            terminal_head: None,
            child_workspaces: None,
        };
        let spawn_observation = format!(
            "{}/observation",
            paths::tool_payload_dir(oid('1').as_str(), 0, "spawn")
        );
        let spawn_tool = ToolRecord {
            request: oid('1'),
            round: 0,
            id: "spawn".to_string(),
            name: "subagent".to_string(),
            declaration_message: "assistant-1".to_string(),
            workspace_name: None,
            input_workspace: None,
            status: ToolStatus::Complete,
            task: None,
            result: Some(ToolResult::Complete {
                observation: spawn_observation,
                proposal: None,
            }),
            workspace_resolution: None,
            files: Vec::new(),
            files_outcome: None,
        };
        assert!(apply(
            &mut store,
            Some(&running.tree),
            &Transition::SubagentSpawn {
                tool: spawn_tool,
                payloads: vec![("observation".to_string(), b"spawned".to_vec())],
                child: bad_child,
            }
        )
        .is_err());
    }

    #[test]
    fn terminal_preconditions_reject_stale_statuses() {
        let mut store = MemoryStore::new();
        let idle_head = root(&mut store);
        let idle_tree = store.read_commit(&idle_head).unwrap().tree;
        let mut idle = request();
        idle.status = RequestStatus::Idle;
        idle.outcome = Some(RequestOutcome::Idle {
            result: None,
            interrupted: false,
        });
        let mut builder = TreeBuilder::from(Some(idle_tree));
        builder.put(
            &paths::request_record_path(idle.id.as_str()),
            Mode::Blob,
            idle.encode(),
        );
        builder.put(paths::ACTIVE_REQUEST, Mode::Blob, oid('1').encode_line());
        let corrupt_active_idle = builder.build(&mut store).unwrap();
        assert!(apply(
            &mut store,
            Some(&corrupt_active_idle),
            &Transition::RequestEscape {
                request: oid('1'),
                reason: None,
            }
        )
        .is_err());

        let mut pending = PublicationRecord {
            id: "publication-1".to_string(),
            key: "key".to_string(),
            descriptor: Descriptor {
                source_base: oid('a'),
                source_head: oid('b'),
                target_base: oid('a'),
                policy: "squash".to_string(),
                implementation: "project-v1".to_string(),
                commit_policy: "single".to_string(),
            },
            planned_head: oid('b'),
            repository: "repo".to_string(),
            refname: "refs/heads/main".to_string(),
            expected_old: Some(oid('a')),
            workspace_name: "main".to_string(),
            status: PublicationStatus::Pending,
            evidence: None,
            observed: None,
        };
        let projection = ids::projection_id(&pending.descriptor.to_value()).unwrap();
        pending.id = ids::publication_id(
            "conversation-1",
            &pending.key,
            &projection,
            &pending.planned_head,
            &pending.repository,
            &pending.refname,
            pending.expected_old.as_ref(),
        )
        .unwrap();
        let publication_id = pending.id.clone();
        let (pending_head, _) = commit_transition(
            &mut store,
            &idle_head,
            Transition::PublicationPending { record: pending },
        );
        let (terminal_head, terminal) = commit_transition(
            &mut store,
            &pending_head,
            Transition::PublicationTerminal {
                publication: publication_id.clone(),
                status: PublicationStatus::Complete,
                evidence: Evidence {
                    kind: "push-success".to_string(),
                    diagnostic: None,
                },
                observed: Some(oid('b')),
            },
        );
        assert!(apply(
            &mut store,
            Some(&terminal.tree),
            &Transition::PublicationTerminal {
                publication: publication_id,
                status: PublicationStatus::Conflict,
                evidence: Evidence {
                    kind: "ref-drift".to_string(),
                    diagnostic: None,
                },
                observed: Some(oid('c')),
            }
        )
        .is_err());
        assert_eq!(
            Conversation::open(&store, &terminal_head)
                .unwrap()
                .publications()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn failed_terminal_drops_cancelling_escape_reason() {
        let mut store = MemoryStore::new();
        let root = root(&mut store);
        let (admitted, _) = commit_transition(
            &mut store,
            &root,
            Transition::RequestAdmit { record: request() },
        );
        let (claimed, _) = commit_transition(
            &mut store,
            &admitted,
            Transition::RequestClaim {
                request: oid('1'),
                latest_message: "message".to_string(),
            },
        );
        let (cancelling, _) = commit_transition(
            &mut store,
            &claimed,
            Transition::RequestEscape {
                request: oid('1'),
                reason: Some("stop".to_string()),
            },
        );
        let (terminal, _) = commit_transition(
            &mut store,
            &cancelling,
            Transition::RequestTerminal {
                request: oid('1'),
                outcome: RequestOutcome::Failed {
                    error: ".caos/requests/error".to_string(),
                },
            },
        );
        let record = Conversation::open(&store, &terminal)
            .unwrap()
            .request(&oid('1'))
            .unwrap()
            .unwrap();
        assert_eq!(record.status, RequestStatus::Failed);
        assert_eq!(record.escape_reason, None);
    }
}
