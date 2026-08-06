use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use caos::chat::{
    archive_user_conversation, automatic_conversation_title, conversation_replay,
    conversation_workspace_diff, describe_tool_set, fresh_conversation_id,
    generate_conversation_title, list_user_conversations, normalize_conversation_title,
    publish_unindexed_conversations, publish_user_conversation, run_chat_turn,
    set_conversation_title, unarchive_user_conversation, ConversationRole, TurnEvent, TurnOptions,
    TurnPhase, UserConversationStatus, UserConversationSummary,
};
use caos::workspace::{
    commit_working_tree, load_conversation_workspace, local_default_branch_tip,
    publish_conversation_pr, remote_default_branch,
};
use caos::{GitTransport, Transport};
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{State, WebviewWindow};

struct AppState {
    repo_dir: PathBuf,
    repo_name: String,
    user: String,
    initial_model: Option<String>,
    discovery_error: Option<String>,
    drafts: Arc<Mutex<HashMap<String, DraftState>>>,
    active_turns: Arc<Mutex<HashSet<String>>>,
}

struct DraftState {
    title: Option<String>,
    started: bool,
    base: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DesktopArgs {
    model: Option<String>,
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
                "-h" | "--help" => return Err(desktop_usage()),
                other => return Err(format!("unknown option {other:?}\n{}", desktop_usage())),
            }
        }
        Ok(parsed)
    }
}

fn desktop_usage() -> String {
    "usage: caos-desktop [--model <model>]".to_string()
}

struct ActiveTurnGuard {
    conversation: String,
    active_turns: Arc<Mutex<HashSet<String>>>,
}

impl ActiveTurnGuard {
    fn start(
        active_turns: Arc<Mutex<HashSet<String>>>,
        conversation: String,
    ) -> Result<Self, String> {
        {
            let mut active = active_turns
                .lock()
                .map_err(|_| "desktop turn state is unavailable".to_string())?;
            if !active.insert(conversation.clone()) {
                return Err(format!("conversation {conversation:?} is already running"));
            }
        }
        Ok(Self {
            conversation,
            active_turns,
        })
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
        let (initial_model, argument_error) = match args {
            Ok(args) => (args.model, None),
            Err(error) => (None, Some(error)),
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
        let user = std::env::var("CAOS_USER")
            .or_else(|_| std::env::var("USER"))
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "caos-desktop".to_string());
        Self {
            repo_dir,
            repo_name,
            user,
            initial_model,
            discovery_error: argument_error.or(repository_error),
            drafts: Arc::new(Mutex::new(HashMap::new())),
            active_turns: Arc::new(Mutex::new(HashSet::new())),
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
    initial_model: Option<String>,
    conversations: Vec<ConversationPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConversationPayload {
    id: String,
    title: String,
    head: String,
    short_head: String,
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
    timestamp_unix: i64,
    role: &'static str,
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
struct TurnCompletionPayload {
    title: String,
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
        tool_use_id: String,
        name: String,
        summary: String,
    },
    ToolResult {
        step_commit: String,
        tool_use_id: String,
        is_error: bool,
        content: String,
    },
    Completed {
        commit: String,
        short_commit: String,
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
                tool_use_id,
                name,
                summary,
            } => Self::ToolCall {
                step_commit,
                tool_use_id,
                name,
                summary,
            },
            TurnEvent::ToolResult {
                step_commit,
                tool_use_id,
                is_error,
                content,
            } => Self::ToolResult {
                step_commit,
                tool_use_id,
                is_error,
                content,
            },
            TurnEvent::Completed(outcome) => Self::Completed {
                commit: outcome.commit,
                short_commit: outcome.short_commit,
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
    publish_unindexed_conversations(transport, user)?;
    list_user_conversations(transport, user, UserConversationStatus::Active)
        .map(|items| items.into_iter().map(ConversationPayload::from).collect())
}

#[tauri::command]
async fn bootstrap(state: State<'_, AppState>) -> Result<BootstrapPayload, String> {
    let repo_dir = state.repo_dir()?;
    let repo_name = state.repo_name.clone();
    let initial_model = state.initial_model.clone();
    let user = state.user.clone();
    run_blocking(move || {
        let transport = GitTransport::discover(&repo_dir)?;
        let conversations = active_conversations(&transport, &user)?;
        Ok(BootstrapPayload {
            repo_name,
            initial_model,
            conversations,
        })
    })
    .await
}

#[tauri::command]
async fn get_conversations(state: State<'_, AppState>) -> Result<Vec<ConversationPayload>, String> {
    let repo_dir = state.repo_dir()?;
    let user = state.user.clone();
    run_blocking(move || {
        let transport = GitTransport::discover(repo_dir)?;
        active_conversations(&transport, &user)
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
        let base = match requested_base {
            Some(requested) => transport
                .resolve_revspec(requested.trim())?
                .ok_or_else(|| format!("cannot resolve commit {requested:?}"))?
                .to_string(),
            None => local_default_branch_tip(&repo_dir)?.1,
        };
        let id = fresh_conversation_id(&transport, &user)?;
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
async fn checkout_conversation(
    state: State<'_, AppState>,
    conversation: String,
) -> Result<String, String> {
    ensure_conversation_idle(&state, &conversation, "checking it out")?;
    let repo_dir = state.repo_dir()?;
    run_blocking(move || {
        let transport = GitTransport::discover(&repo_dir)?;
        let diff = conversation_workspace_diff(&transport, &conversation)?;
        load_conversation_workspace(&diff.head, &repo_dir)?;
        Ok(short_hash(&diff.head).to_string())
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
) -> Result<String, String> {
    ensure_conversation_idle(&state, &conversation, "publishing it")?;
    let repo_dir = state.repo_dir()?;
    run_blocking(move || {
        let transport = GitTransport::discover(&repo_dir)?;
        let diff = conversation_workspace_diff(&transport, &conversation)?;
        if diff.patch.is_empty() {
            return Err("there are no conversation changes to publish".to_string());
        }
        let default_base = remote_default_branch(&repo_dir)?;
        let requested = base
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let pr_base = requested.unwrap_or(&default_base);
        publish_conversation_pr(&conversation, &diff, pr_base, &default_base, &repo_dir)
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
    run_blocking(move || {
        let transport = GitTransport::discover(repo_dir)?;
        let options = TurnOptions {
            base,
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
async fn get_history(
    state: State<'_, AppState>,
    conversation: String,
) -> Result<HistoryPayload, String> {
    if state
        .drafts
        .lock()
        .map_err(|_| "desktop draft state is unavailable".to_string())?
        .contains_key(&conversation)
    {
        return Ok(HistoryPayload {
            turns: Vec::new(),
            turn_events: Vec::new(),
        });
    }
    let repo_dir = state.repo_dir()?;
    run_blocking(move || {
        let transport = GitTransport::discover(repo_dir)?;
        conversation_replay(&transport, &conversation).map(|replay| HistoryPayload {
            turns: replay
                .turns
                .into_iter()
                .map(|turn| HistoryEntryPayload {
                    commit: turn.commit,
                    short_commit: turn.short_commit,
                    timestamp_unix: turn.timestamp_unix,
                    role: match turn.role {
                        ConversationRole::Human => "human",
                        ConversationRole::Agent => "agent",
                    },
                    message: turn.message,
                })
                .collect(),
            turn_events: replay
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
        })
    })
    .await
}

#[tauri::command]
async fn get_diff(state: State<'_, AppState>, conversation: String) -> Result<String, String> {
    if state
        .drafts
        .lock()
        .map_err(|_| "desktop draft state is unavailable".to_string())?
        .contains_key(&conversation)
    {
        return Ok(String::new());
    }
    let repo_dir = state.repo_dir()?;
    run_blocking(move || {
        let transport = GitTransport::discover(repo_dir)?;
        conversation_workspace_diff(&transport, &conversation).map(|diff| diff.patch)
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
    let model = model
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let active_turn =
        ActiveTurnGuard::start(Arc::clone(&state.active_turns), conversation.clone())?;
    tauri::async_runtime::spawn_blocking(move || {
        let _active_turn = active_turn;
        (|| {
            let draft = {
                let mut drafts = drafts
                    .lock()
                    .map_err(|_| "desktop draft state is unavailable".to_string())?;
                drafts.get_mut(&conversation).map(|draft| {
                    draft.started = true;
                    let automatic_title = draft.title.is_none();
                    let title = draft
                        .title
                        .clone()
                        .unwrap_or_else(|| automatic_conversation_title(&message));
                    draft.title = Some(title.clone());
                    (title, draft.base.clone(), automatic_title)
                })
            };
            let is_draft = draft.is_some();
            let (resolved_title, base, generate_title) = match draft {
                Some((title, base, generate_title)) => (title, Some(base), generate_title),
                None => (title, None, false),
            };
            let options = TurnOptions {
                base,
                model,
                ..TurnOptions::default()
            };
            let human_tree = if update_tree {
                Some(commit_working_tree(&message, &repo_dir)?)
            } else {
                None
            };
            let title_task = generate_title.then(|| {
                let repo_dir = repo_dir.clone();
                let options = options.clone();
                let prompt = message.clone();
                std::thread::spawn(move || {
                    GitTransport::discover(repo_dir).and_then(|transport| {
                        generate_conversation_title(&transport, &options, &prompt)
                    })
                })
            });
            let transport = GitTransport::discover(&repo_dir)?;
            let turn = run_chat_turn(
                &transport,
                &options,
                &conversation,
                &message,
                human_tree.as_deref(),
                |event| {
                    let _ = on_event.send(TurnEventPayload::from(event));
                },
            );
            turn?;
            let resolved_title = title_task
                .and_then(|task| task.join().ok())
                .and_then(Result::ok)
                .unwrap_or(resolved_title);
            publish_user_conversation(&transport, &user, &conversation, &resolved_title)?;
            if is_draft {
                drafts
                    .lock()
                    .map_err(|_| "desktop draft state is unavailable".to_string())?
                    .remove(&conversation);
            }
            Ok(TurnCompletionPayload {
                title: resolved_title,
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
            get_conversations,
            new_conversation,
            rename_conversation,
            checkout_conversation,
            default_publish_branch,
            publish_conversation,
            archive_conversation,
            get_archived_conversations,
            restore_conversation,
            get_tools,
            get_history,
            get_diff,
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
    fn desktop_arguments_accept_only_a_model_override() {
        let parsed = DesktopArgs::parse(["--model".to_string(), "test-model".to_string()]).unwrap();
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
            tool_use_id: "tool-1".to_string(),
            is_error: false,
            content: "result".to_string(),
        });
        assert_eq!(
            serde_json::to_value(payload).unwrap(),
            serde_json::json!({
                "kind": "toolResult",
                "stepCommit": "abc1234",
                "toolUseId": "tool-1",
                "isError": false,
                "content": "result"
            })
        );
    }
}
