use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use caos::chat::{
    automatic_conversation_title, conversation_replay, conversation_workspace_diff,
    fresh_conversation_id, list_user_conversations, normalize_conversation_title,
    publish_unindexed_conversations, publish_user_conversation, run_chat_turn,
    set_conversation_title, ConversationRole, TurnEvent, TurnOptions, TurnPhase,
    UserConversationStatus, UserConversationSummary,
};
use caos::GitTransport;
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{State, WebviewWindow};

struct AppState {
    repo_dir: PathBuf,
    repo_name: String,
    user: String,
    discovery_error: Option<String>,
    drafts: Arc<Mutex<HashMap<String, DraftState>>>,
    active_turns: Arc<Mutex<HashSet<String>>>,
}

#[derive(Default)]
struct DraftState {
    title: Option<String>,
    started: bool,
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
    fn discover() -> Self {
        let requested = std::env::var_os("CAOS_REPO")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let (repo_dir, repo_name, discovery_error) = match GitTransport::discover(&requested) {
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
            discovery_error,
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
    let user = state.user.clone();
    run_blocking(move || {
        let transport = GitTransport::discover(&repo_dir)?;
        let conversations = active_conversations(&transport, &user)?;
        Ok(BootstrapPayload {
            repo_name,
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
async fn new_conversation(state: State<'_, AppState>) -> Result<ConversationPayload, String> {
    let repo_dir = state.repo_dir()?;
    let user = state.user.clone();
    let drafts = Arc::clone(&state.drafts);
    run_blocking(move || {
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
        let transport = GitTransport::discover(repo_dir)?;
        let id = fresh_conversation_id(&transport, &user)?;
        drafts
            .lock()
            .map_err(|_| "desktop draft state is unavailable".to_string())?
            .insert(id.clone(), DraftState::default());
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
    if state
        .active_turns
        .lock()
        .map_err(|_| "desktop turn state is unavailable".to_string())?
        .contains(&conversation)
    {
        return Err("finish this conversation's operation before renaming it".to_string());
    }
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
    on_event: Channel<TurnEventPayload>,
) -> Result<(), String> {
    if message.trim().is_empty() {
        return Err("empty message".to_string());
    }
    let title = normalize_conversation_title(&title)?.to_string();
    let repo_dir = state.repo_dir()?;
    let user = state.user.clone();
    let drafts = Arc::clone(&state.drafts);
    let active_turn =
        ActiveTurnGuard::start(Arc::clone(&state.active_turns), conversation.clone())?;
    tauri::async_runtime::spawn_blocking(move || {
        let _active_turn = active_turn;
        (|| {
            let draft_title = {
                let mut drafts = drafts
                    .lock()
                    .map_err(|_| "desktop draft state is unavailable".to_string())?;
                drafts.get_mut(&conversation).map(|draft| {
                    draft.started = true;
                    let title = draft
                        .title
                        .clone()
                        .unwrap_or_else(|| automatic_conversation_title(&message));
                    draft.title = Some(title.clone());
                    title
                })
            };
            let is_draft = draft_title.is_some();
            let resolved_title = draft_title.unwrap_or(title);
            let transport = GitTransport::discover(&repo_dir)?;
            run_chat_turn(
                &transport,
                &TurnOptions::default(),
                &conversation,
                &message,
                None,
                |event| {
                    let _ = on_event.send(TurnEventPayload::from(event));
                },
            )?;
            publish_user_conversation(&transport, &user, &conversation, &resolved_title)?;
            if is_draft {
                drafts
                    .lock()
                    .map_err(|_| "desktop draft state is unavailable".to_string())?
                    .remove(&conversation);
            }
            Ok(())
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
        .manage(AppState::discover())
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            get_conversations,
            new_conversation,
            rename_conversation,
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
    use super::{short_hash, ActiveTurnGuard, TurnEventPayload};
    use caos::chat::TurnEvent;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    #[test]
    fn hashes_are_safe_when_short() {
        assert_eq!(short_hash("123456789"), "1234567");
        assert_eq!(short_hash("abc"), "abc");
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
