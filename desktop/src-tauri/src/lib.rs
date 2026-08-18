use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use caos::chat::{
    archive_user_conversation, automatic_conversation_title, compare_and_set_conversation_title,
    conversation_load, conversation_load_at, conversation_reference, conversation_snapshot,
    describe_tool_set, first_available_conversation_name, fork_conversation, fresh_conversation_id,
    generate_conversation_title, interrupt_request, invite_user_to_conversation,
    list_user_conversations, normalize_conversation_title, normalized_username,
    publish_user_conversation, resume_request, run_chat_turn, set_conversation_title,
    submit_interjection, unarchive_user_conversation, ConversationLoad, ConversationRole,
    InviteOutcome, TurnEvent, TurnOptions, TurnPhase, UserConversationStatus,
    UserConversationSummary, DEFAULT_MODEL,
};
use caos::workspace::{
    commit_working_tree, fetch_remote_branch_tip, load_conversation_workspace,
    local_default_branch_tip, prepare_publish_workspace, publish_conversation_pr,
    publish_merge_target, remote_default_branch,
};
use caos::{GitTransport, Transport};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{State, WebviewWindow};

struct AppState {
    repo_dir: PathBuf,
    repo_name: String,
    user: String,
    initial_model: String,
    discovery_error: Option<String>,
    drafts: Arc<Mutex<HashMap<String, DraftState>>>,
    active_turns: Arc<Mutex<HashSet<String>>>,
    reconciling_requests: Arc<Mutex<HashSet<String>>>,
}

struct DraftState {
    title: Option<String>,
    started: bool,
    base: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DesktopArgs {
    model: Option<String>,
    username: Option<String>,
}

impl DesktopArgs {
    fn parse(raw: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut args = raw.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--model" => {
                    let model = args
                        .next()
                        .ok_or_else(|| "--model needs a value".to_string())?;
                    parsed.model = Some(model);
                }
                "--username" => {
                    let username = args
                        .next()
                        .ok_or_else(|| "--username needs a value".to_string())?;
                    parsed.username = Some(username);
                }
                "-h" | "--help" => return Err(desktop_usage()),
                other => return Err(format!("unknown option {other:?}\n{}", desktop_usage())),
            }
        }
        Ok(parsed)
    }
}

fn desktop_usage() -> String {
    "usage: caos-desktop [--username <name>] [--model <model>]".to_string()
}

struct ActiveTurnGuard {
    conversation: String,
    active_turns: Arc<Mutex<HashSet<String>>>,
}

impl ActiveTurnGuard {
    fn reserve(
        active_turns: Arc<Mutex<HashSet<String>>>,
        conversation: String,
    ) -> Result<Option<Self>, String> {
        let reserved = active_turns
            .lock()
            .map_err(|_| "desktop turn state is unavailable".to_string())?
            .insert(conversation.clone());
        Ok(reserved.then_some(Self {
            conversation,
            active_turns,
        }))
    }

    fn start(
        active_turns: Arc<Mutex<HashSet<String>>>,
        conversation: String,
    ) -> Result<Self, String> {
        Self::reserve(active_turns, conversation.clone())?
            .ok_or_else(|| format!("conversation {conversation:?} is already running"))
    }
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active_turns.lock() {
            active.remove(&self.conversation);
        }
    }
}

impl AppState {
    fn discover(args: Result<DesktopArgs, String>) -> Self {
        let (initial_model, explicit_user, argument_error) = match args {
            Ok(args) => (
                args.model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
                args.username,
                None,
            ),
            Err(error) => (DEFAULT_MODEL.to_string(), None, Some(error)),
        };
        let requested = std::env::var_os("CAOS_REPO")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let (repo_dir, repo_name, repository_error) = match GitTransport::discover(&requested) {
            Ok(transport) => {
                let repo_dir = transport.work_dir().to_path_buf();
                let repo_name = repo_dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("repository")
                    .to_string();
                (repo_dir, repo_name, None)
            }
            Err(error) => {
                let repo_name = requested
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("repository")
                    .to_string();
                (requested, repo_name, Some(error))
            }
        };
        let (user, user_error) = match explicit_user {
            Some(user) => match normalized_username(&user) {
                Some(user) => (user, None),
                None => (
                    String::new(),
                    Some("--username must be 1-126 UTF-8 bytes and contain no control or invisible formatting characters".to_string()),
                ),
            },
            None => match std::env::var("USER") {
                Ok(user) => match normalized_username(&user) {
                    Some(user) => (user, None),
                    None => (
                        String::new(),
                        Some("$USER is not a usable identity; pass --username explicitly".to_string()),
                    ),
                },
                Err(std::env::VarError::NotPresent) => (
                    String::new(),
                    Some("--username is required when $USER is not set".to_string()),
                ),
                Err(std::env::VarError::NotUnicode(_)) => (
                    String::new(),
                    Some("$USER is not valid UTF-8; pass --username explicitly".to_string()),
                ),
            },
        };
        Self {
            repo_dir,
            repo_name,
            user,
            initial_model,
            discovery_error: argument_error.or(user_error).or(repository_error),
            drafts: Arc::new(Mutex::new(HashMap::new())),
            active_turns: Arc::new(Mutex::new(HashSet::new())),
            reconciling_requests: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn repo_dir(&self) -> Result<PathBuf, String> {
        if let Some(error) = &self.discovery_error {
            return Err(error.clone());
        }
        Ok(self.repo_dir.clone())
    }
}

async fn run_blocking<T, F>(operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(operation)
        .await
        .map_err(|error| format!("desktop worker failed: {error}"))?
}

#[tauri::command]
fn set_ui_zoom(window: WebviewWindow, scale: f64) -> Result<(), String> {
    if !(0.8..=1.6).contains(&scale) {
        return Err(format!("UI zoom {scale} is outside the supported range"));
    }
    window
        .set_zoom(scale)
        .map_err(|error| format!("could not change UI zoom: {error}"))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapPayload {
    repo_name: String,
    user: String,
    default_model: &'static str,
    initial_model: String,
    conversations: Vec<ConversationPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConversationPayload {
    id: String,
    title: String,
    head: String,
    short_head: String,
    parent: Option<String>,
    draft: bool,
    started: bool,
}

impl ConversationPayload {
    fn draft(id: String, title: String) -> Self {
        Self {
            id,
            title,
            head: String::new(),
            short_head: String::new(),
            parent: None,
            draft: true,
            started: false,
        }
    }
}

impl From<UserConversationSummary> for ConversationPayload {
    fn from(summary: UserConversationSummary) -> Self {
        Self {
            short_head: short_hash(&summary.head).to_string(),
            id: summary.id,
            title: summary.title,
            head: summary.head,
            parent: summary.parent,
            draft: false,
            started: true,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryEntryPayload {
    commit: String,
    short_commit: String,
    author: String,
    role: &'static str,
    model: Option<String>,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryTurnEventsPayload {
    turn_commit: String,
    events: Vec<TurnEventPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryPayload {
    turns: Vec<HistoryEntryPayload>,
    turn_events: Vec<HistoryTurnEventsPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConversationLoadPayload {
    head: String,
    short_head: String,
    status: String,
    request: Option<String>,
    interrupted: bool,
    history: HistoryPayload,
    patch: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObservedConversationPayload {
    id: String,
    head: String,
    status: Option<String>,
    request: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConversationPollPayload {
    conversations: Vec<ConversationPayload>,
    loads: HashMap<String, ConversationLoadPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnCompletionPayload {
    title: String,
    interjected: bool,
    interrupted: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConversationReferencePayload {
    refname: String,
    head: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolSetPayload {
    source: String,
    tools: Vec<ToolPayload>,
}

#[derive(Serialize)]
struct ToolPayload {
    name: String,
    docs: String,
    image: String,
}

#[derive(Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum TurnEventPayload {
    Submitted {
        commit: String,
    },
    PhaseStarted {
        phase: &'static str,
    },
    PhaseComplete {
        label: String,
        elapsed_secs: f64,
    },
    Status {
        text: String,
    },
    AssistantText {
        text: String,
    },
    ToolCall {
        step_commit: String,
        request: String,
        round: u64,
        tool_use_id: String,
        name: String,
        summary: String,
    },
    ToolResult {
        step_commit: String,
        request: String,
        round: u64,
        tool_use_id: String,
        is_error: bool,
        content: String,
    },
    Completed {
        commit: String,
        short_commit: String,
        interrupted: bool,
    },
}

impl From<TurnEvent> for TurnEventPayload {
    fn from(event: TurnEvent) -> Self {
        match event {
            TurnEvent::PhaseStarted(phase) => Self::PhaseStarted {
                phase: phase_name(phase),
            },
            TurnEvent::PhaseComplete {
                label,
                elapsed_secs,
            } => Self::PhaseComplete {
                label,
                elapsed_secs,
            },
            TurnEvent::Status(text) => Self::Status { text },
            TurnEvent::AssistantText(text) => Self::AssistantText { text },
            TurnEvent::ToolCall {
                step_commit,
                request,
                round,
                tool_use_id,
                name,
                summary,
            } => Self::ToolCall {
                step_commit,
                request,
                round,
                tool_use_id,
                name,
                summary,
            },
            TurnEvent::ToolResult {
                step_commit,
                request,
                round,
                tool_use_id,
                is_error,
                content,
            } => Self::ToolResult {
                step_commit,
                request,
                round,
                tool_use_id,
                is_error,
                content,
            },
            TurnEvent::Completed(outcome) => Self::Completed {
                commit: outcome.commit,
                short_commit: outcome.short_commit,
                interrupted: outcome.interrupted,
            },
        }
    }
}

fn phase_name(phase: TurnPhase) -> &'static str {
    match phase {
        TurnPhase::System => "system",
        TurnPhase::Model => "model",
    }
}

fn short_hash(hash: &str) -> &str {
    hash.get(..7).unwrap_or(hash)
}

fn request_is_active(status: &str) -> bool {
    matches!(status, "queued" | "running")
}

impl ConversationLoadPayload {
    fn from_load(load: ConversationLoad, current_user: &str) -> Self {
        let head = load.snapshot.head.clone();
        Self {
            short_head: short_hash(&head).to_string(),
            head,
            status: load.snapshot.status,
            request: load.snapshot.request,
            interrupted: load.snapshot.interrupted,
            history: HistoryPayload {
                turns: load
                    .replay
                    .turns
                    .into_iter()
                    .map(|turn| HistoryEntryPayload {
                        commit: turn.commit,
                        short_commit: turn.short_commit,
                        role: match turn.role {
                            ConversationRole::Human if turn.author != current_user => "peer",
                            ConversationRole::Human => "human",
                            ConversationRole::Agent => "agent",
                        },
                        author: turn.author,
                        model: turn.model,
                        message: turn.message,
                    })
                    .collect(),
                turn_events: load
                    .replay
                    .turn_events
                    .into_iter()
                    .map(|turn| HistoryTurnEventsPayload {
                        turn_commit: turn.turn_commit,
                        events: turn
                            .events
                            .into_iter()
                            .map(TurnEventPayload::from)
                            .collect(),
                    })
                    .collect(),
            },
            patch: load.workspace_diff.patch,
        }
    }
}

fn schedule_resume(
    repo_dir: PathBuf,
    request: String,
    reconciling_requests: Arc<Mutex<HashSet<String>>>,
) -> Result<(), String> {
    {
        let mut reconciling = reconciling_requests
            .lock()
            .map_err(|_| "desktop request state is unavailable".to_string())?;
        if !reconciling.insert(request.clone()) {
            return Ok(());
        }
    }
    std::thread::spawn(move || {
        let _ = GitTransport::discover(repo_dir)
            .and_then(|transport| resume_request(&transport, &request));
        if let Ok(mut reconciling) = reconciling_requests.lock() {
            reconciling.remove(&request);
        }
    });
    Ok(())
}

fn reconcile_load(
    repo_dir: &std::path::Path,
    active_turns: &Arc<Mutex<HashSet<String>>>,
    reconciling_requests: &Arc<Mutex<HashSet<String>>>,
    conversation: &str,
    load: &ConversationLoad,
) -> Result<(), String> {
    if !request_is_active(&load.snapshot.status)
        || active_turns
            .lock()
            .map_err(|_| "desktop turn state is unavailable".to_string())?
            .contains(conversation)
    {
        return Ok(());
    }
    let request = load.snapshot.request.clone().ok_or_else(|| {
        format!("active conversation {conversation:?} has no durably recorded request")
    })?;
    schedule_resume(
        repo_dir.to_path_buf(),
        request,
        Arc::clone(reconciling_requests),
    )
}

fn ensure_conversation_idle(
    state: &AppState,
    conversation: &str,
    action: &str,
) -> Result<(), String> {
    if state
        .active_turns
        .lock()
        .map_err(|_| "desktop turn state is unavailable".to_string())?
        .contains(conversation)
    {
        return Err(format!(
            "finish this conversation's operation before {action}"
        ));
    }
    Ok(())
}

fn active_conversations(
    transport: &GitTransport,
    user: &str,
) -> Result<Vec<ConversationPayload>, String> {
    list_user_conversations(transport, user, UserConversationStatus::Active)
        .map(|items| items.into_iter().map(ConversationPayload::from).collect())
}

#[tauri::command]
async fn bootstrap(state: State<'_, AppState>) -> Result<BootstrapPayload, String> {
    let repo_dir = state.repo_dir()?;
    let repo_name = state.repo_name.clone();
    let initial_model = state.initial_model.clone();
    let user = state.user.clone();
    let payload_user = user.clone();
    run_blocking(move || {
        let transport = GitTransport::discover(&repo_dir)?;
        let conversations = active_conversations(&transport, &user)?;
        Ok(BootstrapPayload {
            repo_name,
            user: payload_user,
            default_model: DEFAULT_MODEL,
            initial_model,
            conversations,
        })
    })
    .await
}

#[tauri::command]
async fn get_conversation(
    state: State<'_, AppState>,
    conversation: String,
) -> Result<Option<ConversationLoadPayload>, String> {
    if state
        .drafts
        .lock()
        .map_err(|_| "desktop draft state is unavailable".to_string())?
        .contains_key(&conversation)
    {
        return Ok(None);
    }
    let repo_dir = state.repo_dir()?;
    let user = state.user.clone();
    let active_turns = Arc::clone(&state.active_turns);
    let reconciling_requests = Arc::clone(&state.reconciling_requests);
    run_blocking(move || {
        let transport = GitTransport::discover(&repo_dir)?;
        let Some(load) = conversation_load(&transport, &conversation)? else {
            return Ok(None);
        };
        reconcile_load(
            &repo_dir,
            &active_turns,
            &reconciling_requests,
            &conversation,
            &load,
        )?;
        Ok(Some(ConversationLoadPayload::from_load(load, &user)))
    })
    .await
}

#[tauri::command]
async fn poll_conversations(
    state: State<'_, AppState>,
    observed: Vec<ObservedConversationPayload>,
) -> Result<ConversationPollPayload, String> {
    let repo_dir = state.repo_dir()?;
    let user = state.user.clone();
    let active_turns = Arc::clone(&state.active_turns);
    let reconciling_requests = Arc::clone(&state.reconciling_requests);
    run_blocking(move || {
        let transport = GitTransport::discover(&repo_dir)?;
        let mut observed_heads = HashMap::new();
        for item in observed {
            if item.status.as_deref().is_some_and(request_is_active)
                && !active_turns
                    .lock()
                    .map_err(|_| "desktop turn state is unavailable".to_string())?
                    .contains(&item.id)
            {
                let request = item.request.ok_or_else(|| {
                    format!(
                        "active conversation {:?} has no durably recorded request",
                        item.id
                    )
                })?;
                schedule_resume(repo_dir.clone(), request, Arc::clone(&reconciling_requests))?;
            }
            observed_heads.insert(item.id, item.head);
        }
        let summaries = list_user_conversations(&transport, &user, UserConversationStatus::Active)?;
        let mut loads = HashMap::new();
        for summary in &summaries {
            if observed_heads.get(&summary.id) == Some(&summary.head) {
                continue;
            }
            let load = conversation_load(&transport, &summary.id)?.ok_or_else(|| {
                format!("conversation {:?} disappeared during refresh", summary.id)
            })?;
            reconcile_load(
                &repo_dir,
                &active_turns,
                &reconciling_requests,
                &summary.id,
                &load,
            )?;
            loads.insert(
                summary.id.clone(),
                ConversationLoadPayload::from_load(load, &user),
            );
        }
        Ok(ConversationPollPayload {
            conversations: summaries
                .into_iter()
                .map(ConversationPayload::from)
                .collect(),
            loads,
        })
    })
    .await
}

#[tauri::command]
async fn new_conversation(
    state: State<'_, AppState>,
    base: Option<String>,
) -> Result<ConversationPayload, String> {
    let repo_dir = state.repo_dir()?;
    let user = state.user.clone();
    let drafts = Arc::clone(&state.drafts);
    run_blocking(move || {
        let requested_base = base.filter(|value| !value.trim().is_empty());
        if requested_base.is_none() {
            if let Some((id, title)) = drafts
                .lock()
                .map_err(|_| "desktop draft state is unavailable".to_string())?
                .iter()
                .find(|(_, draft)| !draft.started)
                .map(|(id, draft)| {
                    (
                        id.clone(),
                        draft
                            .title
                            .clone()
                            .unwrap_or_else(|| "New conversation".to_string()),
                    )
                })
            {
                return Ok(ConversationPayload::draft(id, title));
            }
        }
        let transport = GitTransport::discover(&repo_dir)?;
        let id = fresh_conversation_id(&transport, &user)?;
        if let Some(requested) = requested_base {
            let source = transport
                .resolve_revspec(requested.trim())?
                .ok_or_else(|| format!("cannot resolve commit {requested:?}"))?
                .to_string();
            let summaries =
                list_user_conversations(&transport, &user, UserConversationStatus::Active)?;
            let title = first_available_conversation_name(
                summaries
                    .iter()
                    .map(|conversation| conversation.title.as_str()),
            );
            let fork = fork_conversation(&transport, &user, &id, &title, &source)?;
            conversation_load_at(&transport, &id, &fork)?;
            return list_user_conversations(&transport, &user, UserConversationStatus::Active)?
                .into_iter()
                .find(|conversation| conversation.id == id)
                .map(ConversationPayload::from)
                .ok_or_else(|| format!("forked conversation {id:?} was not indexed"));
        }
        let base = local_default_branch_tip(&repo_dir)?.1;
        drafts
            .lock()
            .map_err(|_| "desktop draft state is unavailable".to_string())?
            .insert(
                id.clone(),
                DraftState {
                    title: None,
                    started: false,
                    base,
                },
            );
        Ok(ConversationPayload::draft(
            id,
            "New conversation".to_string(),
        ))
    })
    .await
}

#[tauri::command]
async fn rename_conversation(
    state: State<'_, AppState>,
    conversation: String,
    title: String,
) -> Result<String, String> {
    let title = normalize_conversation_title(&title)?.to_string();
    ensure_conversation_idle(&state, &conversation, "renaming it")?;
    {
        let mut drafts = state
            .drafts
            .lock()
            .map_err(|_| "desktop draft state is unavailable".to_string())?;
        if let Some(draft) = drafts.get_mut(&conversation) {
            draft.title = Some(title.clone());
            return Ok(title);
        }
    }

    let repo_dir = state.repo_dir()?;
    run_blocking(move || {
        let transport = GitTransport::discover(repo_dir)?;
        set_conversation_title(&transport, &conversation, &title)?;
        Ok(title)
    })
    .await
}

#[tauri::command]
async fn invite_conversation(
    state: State<'_, AppState>,
    conversation: String,
    username: String,
) -> Result<String, String> {
    let repo_dir = state.repo_dir()?;
    run_blocking(move || {
        let transport = GitTransport::discover(repo_dir)?;
        match invite_user_to_conversation(&transport, &username, &conversation)? {
            InviteOutcome::Created => Ok(format!(
                "Invited username {username:?}. They must select that exact case-sensitive identity."
            )),
            InviteOutcome::AlreadyActive => {
                Ok(format!("Username {username:?} already has this conversation active."))
            }
            InviteOutcome::Archived => Ok(format!(
                "Username {username:?} has archived this conversation; their choice was preserved."
            )),
        }
    })
    .await
}

#[tauri::command]
async fn get_conversation_reference(
    state: State<'_, AppState>,
    conversation: String,
) -> Result<ConversationReferencePayload, String> {
    let repo_dir = state.repo_dir()?;
    run_blocking(move || {
        let transport = GitTransport::discover(repo_dir)?;
        let (refname, head) = conversation_reference(&transport, &conversation)?;
        Ok(ConversationReferencePayload { refname, head })
    })
    .await
}

#[tauri::command]
async fn interrupt_conversation(
    state: State<'_, AppState>,
    conversation: String,
) -> Result<String, String> {
    let repo_dir = state.repo_dir()?;
    run_blocking(move || {
        let transport = GitTransport::discover(repo_dir)?;
        let wait_for_admission = conversation_snapshot(&transport, &conversation)?
            .is_none_or(|snapshot| snapshot.request.is_none());
        let attempts = if wait_for_admission { 40 } else { 1 };
        let mut last_error = None;
        for attempt in 0..attempts {
            match interrupt_request(&transport, &conversation) {
                Ok(commit) => return Ok(commit),
                Err(error) => last_error = Some(error),
            }
            if attempt + 1 < attempts {
                std::thread::sleep(std::time::Duration::from_millis(125));
            }
        }
        Err(last_error.unwrap_or_else(|| "recording Escape failed".to_string()))
    })
    .await
}

#[tauri::command]
async fn checkout_conversation(
    state: State<'_, AppState>,
    conversation: String,
) -> Result<String, String> {
    ensure_conversation_idle(&state, &conversation, "checking it out")?;
    let repo_dir = state.repo_dir()?;
    run_blocking(move || {
        let transport = GitTransport::discover(&repo_dir)?;
        let load = conversation_load(&transport, &conversation)?
            .ok_or_else(|| format!("no conversation {conversation:?}"))?;
        load_conversation_workspace(&load.workspace_diff.head, &repo_dir)?;
        Ok(short_hash(&load.workspace_diff.head).to_string())
    })
    .await
}

#[tauri::command]
async fn default_publish_branch(state: State<'_, AppState>) -> Result<String, String> {
    let repo_dir = state.repo_dir()?;
    run_blocking(move || remote_default_branch(&repo_dir)).await
}

#[tauri::command]
async fn publish_conversation(
    state: State<'_, AppState>,
    conversation: String,
    base: Option<String>,
    model: Option<String>,
    on_event: Channel<TurnEventPayload>,
) -> Result<String, String> {
    ensure_conversation_idle(&state, &conversation, "publishing it")?;
    let repo_dir = state.repo_dir()?;
    let user = state.user.clone();
    let model = model
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| state.initial_model.clone());
    let active_turn =
        ActiveTurnGuard::start(Arc::clone(&state.active_turns), conversation.clone())?;
    run_blocking(move || {
        let _active_turn = active_turn;
        let transport = GitTransport::discover(&repo_dir)?;
        let load = conversation_load(&transport, &conversation)?
            .ok_or_else(|| format!("no conversation {conversation:?}"))?;
        if request_is_active(&load.snapshot.status) {
            return Err("finish this conversation's operation before publishing it".to_string());
        }
        if load.workspace_diff.patch.is_empty() {
            return Err("there are no conversation changes to publish".to_string());
        }
        let default_base = remote_default_branch(&repo_dir)?;
        let pr_base = base
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&default_base)
            .to_string();
        let base_commit = fetch_remote_branch_tip(&pr_base, &repo_dir)?;
        let target = publish_merge_target(
            &load.workspace_diff.base_commit,
            &base_commit,
            pr_base != default_base,
            &repo_dir,
        )?;
        transport.ensure_pushed(&target)?;
        let message = format!(
            "Prepare this conversation for publication. First call the existing `merge` tool \
             with `theirs` exactly `{target}`. Resolve every entry in `.caos/conflicts`, then \
             build and test. Finish only when the workspace is ready to publish."
        );
        let options = TurnOptions {
            model: Some(model),
            username: Some(user),
            ..TurnOptions::default()
        };
        let outcome = run_chat_turn(
            &transport,
            &options,
            &conversation,
            &message,
            None,
            |_| {},
            |event| {
                let _ = on_event.send(TurnEventPayload::from(event));
            },
        )?;
        let workspace = prepare_publish_workspace(&outcome.commit, &target, &repo_dir)?;
        let _ = on_event.send(TurnEventPayload::from(TurnEvent::Completed(outcome)));
        publish_conversation_pr(&conversation, &workspace, &pr_base, &base_commit, &repo_dir)
    })
    .await
}

#[tauri::command]
async fn archive_conversation(
    state: State<'_, AppState>,
    conversation: String,
) -> Result<(), String> {
    ensure_conversation_idle(&state, &conversation, "archiving it")?;
    if state
        .drafts
        .lock()
        .map_err(|_| "desktop draft state is unavailable".to_string())?
        .remove(&conversation)
        .is_some()
    {
        return Ok(());
    }
    let repo_dir = state.repo_dir()?;
    let user = state.user.clone();
    run_blocking(move || {
        let transport = GitTransport::discover(repo_dir)?;
        archive_user_conversation(&transport, &user, &conversation)
    })
    .await
}

#[tauri::command]
async fn get_archived_conversations(
    state: State<'_, AppState>,
) -> Result<Vec<ConversationPayload>, String> {
    let repo_dir = state.repo_dir()?;
    let user = state.user.clone();
    run_blocking(move || {
        let transport = GitTransport::discover(repo_dir)?;
        list_user_conversations(&transport, &user, UserConversationStatus::Archived)
            .map(|items| items.into_iter().map(ConversationPayload::from).collect())
    })
    .await
}

#[tauri::command]
async fn restore_conversation(
    state: State<'_, AppState>,
    conversation: String,
) -> Result<ConversationPayload, String> {
    let repo_dir = state.repo_dir()?;
    let user = state.user.clone();
    run_blocking(move || {
        let transport = GitTransport::discover(&repo_dir)?;
        unarchive_user_conversation(&transport, &user, &conversation)?;
        list_user_conversations(&transport, &user, UserConversationStatus::Active)?
            .into_iter()
            .find(|item| item.id == conversation)
            .map(ConversationPayload::from)
            .ok_or_else(|| format!("restored conversation {conversation:?} was not found"))
    })
    .await
}

#[tauri::command]
async fn get_tools(
    state: State<'_, AppState>,
    conversation: String,
) -> Result<ToolSetPayload, String> {
    let base = state
        .drafts
        .lock()
        .map_err(|_| "desktop draft state is unavailable".to_string())?
        .get(&conversation)
        .map(|draft| draft.base.clone());
    let repo_dir = state.repo_dir()?;
    let user = state.user.clone();
    let model = state.initial_model.clone();
    run_blocking(move || {
        let transport = GitTransport::discover(repo_dir)?;
        let options = TurnOptions {
            base,
            model: Some(model),
            username: Some(user),
            ..TurnOptions::default()
        };
        let tools = describe_tool_set(&transport, &conversation, &options)?;
        Ok(ToolSetPayload {
            source: tools.source,
            tools: tools
                .tools
                .into_iter()
                .map(|tool| ToolPayload {
                    name: tool.name,
                    docs: tool.docs,
                    image: tool.image,
                })
                .collect(),
        })
    })
    .await
}

#[tauri::command]
async fn send_message(
    state: State<'_, AppState>,
    conversation: String,
    message: String,
    title: String,
    model: Option<String>,
    update_tree: bool,
    on_event: Channel<TurnEventPayload>,
) -> Result<TurnCompletionPayload, String> {
    if message.trim().is_empty() {
        return Err("empty message".to_string());
    }
    let title = normalize_conversation_title(&title)?.to_string();
    let repo_dir = state.repo_dir()?;
    let user = state.user.clone();
    let drafts = Arc::clone(&state.drafts);
    let active_turns = Arc::clone(&state.active_turns);
    let model = model
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| state.initial_model.clone());
    let active_turn = ActiveTurnGuard::reserve(active_turns, conversation.clone())?;
    tauri::async_runtime::spawn_blocking(move || {
        (|| {
            let transport = GitTransport::discover(&repo_dir)?;
            let remotely_active = conversation_snapshot(&transport, &conversation)?
                .is_some_and(|snapshot| request_is_active(&snapshot.status));
            let human_tree = if update_tree {
                Some(commit_working_tree(&message, &repo_dir)?)
            } else {
                None
            };
            let options = TurnOptions {
                model: Some(model),
                username: Some(user.clone()),
                ..TurnOptions::default()
            };
            if active_turn.is_none() || remotely_active {
                let commit = submit_interjection(
                    &transport,
                    &options,
                    &conversation,
                    &message,
                    human_tree.as_deref(),
                )?;
                let _ = on_event.send(TurnEventPayload::Submitted { commit });
                return Ok(TurnCompletionPayload {
                    title,
                    interjected: true,
                    interrupted: false,
                });
            }

            let _active_turn = active_turn.expect("idle turns reserve their conversation");
            let draft = {
                let mut drafts = drafts
                    .lock()
                    .map_err(|_| "desktop draft state is unavailable".to_string())?;
                drafts.get_mut(&conversation).map(|draft| {
                    draft.started = true;
                    (draft.title.clone(), draft.base.clone())
                })
            };
            let is_draft = draft.is_some();
            let (requested_title, base) = match draft {
                Some((title, base)) => (title, Some(base)),
                None => (None, None),
            };
            let options = TurnOptions { base, ..options };
            let fallback_title = automatic_conversation_title(&message);
            let title_task = (is_draft && requested_title.is_none()).then(|| {
                let repo_dir = repo_dir.clone();
                let options = options.clone();
                let prompt = message.clone();
                std::thread::spawn(move || {
                    GitTransport::discover(repo_dir).and_then(|transport| {
                        generate_conversation_title(&transport, &options, &prompt)
                    })
                })
            });
            let turn = run_chat_turn(
                &transport,
                &options,
                &conversation,
                &message,
                human_tree.as_deref(),
                |commit| {
                    let _ = on_event.send(TurnEventPayload::Submitted {
                        commit: commit.to_string(),
                    });
                },
                |event| {
                    let _ = on_event.send(TurnEventPayload::from(event));
                },
            );
            if is_draft && conversation_snapshot(&transport, &conversation)?.is_some() {
                drafts
                    .lock()
                    .map_err(|_| "desktop draft state is unavailable".to_string())?
                    .remove(&conversation);
            }
            let outcome = turn?;
            let generated_title = title_task
                .and_then(|task| task.join().ok())
                .and_then(Result::ok);
            publish_user_conversation(&transport, &user, &conversation, &fallback_title)?;
            if let Some(desired_title) = requested_title.or(generated_title) {
                if desired_title != fallback_title {
                    let _ = compare_and_set_conversation_title(
                        &transport,
                        &conversation,
                        &fallback_title,
                        &desired_title,
                    )?;
                }
            }
            let resolved_title =
                list_user_conversations(&transport, &user, UserConversationStatus::Active)?
                    .into_iter()
                    .find(|summary| summary.id == conversation)
                    .map(|summary| summary.title)
                    .unwrap_or(fallback_title);
            let interrupted = outcome.interrupted;
            let _ = on_event.send(TurnEventPayload::from(TurnEvent::Completed(outcome)));
            Ok(TurnCompletionPayload {
                title: resolved_title,
                interjected: false,
                interrupted,
            })
        })()
    })
    .await
    .map_err(|error| format!("desktop turn worker failed: {error}"))?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(true)
                .build(),
        )
        .manage(AppState::discover(DesktopArgs::parse(
            std::env::args().skip(1),
        )))
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            get_conversation,
            poll_conversations,
            new_conversation,
            rename_conversation,
            invite_conversation,
            get_conversation_reference,
            interrupt_conversation,
            checkout_conversation,
            default_publish_branch,
            publish_conversation,
            archive_conversation,
            get_archived_conversations,
            restore_conversation,
            get_tools,
            send_message,
            set_ui_zoom
        ])
        .run(tauri::generate_context!())
        .expect("error while running CAOS desktop");
}

#[cfg(test)]
mod tests {
    use super::{short_hash, ActiveTurnGuard, DesktopArgs, TurnEventPayload};
    use caos::chat::TurnEvent;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    #[test]
    fn hashes_are_safe_when_short() {
        assert_eq!(short_hash("123456789"), "1234567");
        assert_eq!(short_hash("abc"), "abc");
    }

    #[test]
    fn desktop_arguments_accept_identity_and_model() {
        let parsed = DesktopArgs::parse([
            "--username".to_string(),
            "Alice".to_string(),
            "--model".to_string(),
            "test-model".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed.username.as_deref(), Some("Alice"));
        assert_eq!(parsed.model.as_deref(), Some("test-model"));
        assert!(DesktopArgs::parse(["--base".to_string(), "main".to_string()]).is_err());
        assert!(DesktopArgs::parse(["--model".to_string()]).is_err());
    }

    #[test]
    fn active_turns_are_exclusive_and_clear_on_drop() {
        let active = Arc::new(Mutex::new(HashSet::new()));
        let guard = ActiveTurnGuard::start(Arc::clone(&active), "talk-1".to_string()).unwrap();
        assert!(active.lock().unwrap().contains("talk-1"));
        assert!(ActiveTurnGuard::start(Arc::clone(&active), "talk-1".to_string()).is_err());
        drop(guard);
        assert!(active.lock().unwrap().is_empty());
    }

    #[test]
    fn turn_event_payloads_are_tagged_without_null_fields() {
        let payload = TurnEventPayload::from(TurnEvent::ToolResult {
            step_commit: "abc1234".to_string(),
            request: "request-1".to_string(),
            round: 2,
            tool_use_id: "tool-1".to_string(),
            is_error: false,
            content: "result".to_string(),
        });
        assert_eq!(
            serde_json::to_value(payload).unwrap(),
            serde_json::json!({
                "kind": "toolResult",
                "stepCommit": "abc1234",
                "request": "request-1",
                "round": 2,
                "toolUseId": "tool-1",
                "isError": false,
                "content": "result"
            })
        );
        let submitted = TurnEventPayload::Submitted {
            commit: "def5678".to_string(),
        };
        assert_eq!(
            serde_json::to_value(submitted).unwrap(),
            serde_json::json!({ "kind": "submitted", "commit": "def5678" })
        );
    }
}
