use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use caos::chat::{
    conversation_replay, conversation_workspace_diff, list_user_conversations,
    publish_unindexed_conversations, publish_user_conversation, run_chat_turn,
    set_conversation_title, ConversationRole, TurnEvent, TurnOptions, TurnPhase,
    UserConversationStatus, UserConversationSummary,
};
use caos::{GitTransport, Transport};
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
    repo_path: String,
    conversations: Vec<ConversationPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConversationPayload {
    id: String,
    title: String,
    head: String,
    short_head: String,
    updated_unix: i64,
    draft: bool,
    started: bool,
}

impl From<UserConversationSummary> for ConversationPayload {
    fn from(summary: UserConversationSummary) -> Self {
        Self {
            short_head: short_hash(&summary.head).to_string(),
            id: summary.id,
            title: summary.title,
            head: summary.head,
            updated_unix: summary.updated_unix,
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
    author: String,
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
struct DiffPayload {
    base_commit: String,
    head: String,
    patch: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnOutcomePayload {
    conversation: String,
    commit: String,
    short_commit: String,
    title: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnEventPayload {
    kind: &'static str,
    phase: Option<&'static str>,
    label: Option<String>,
    text: Option<String>,
    elapsed_secs: Option<f64>,
    step_commit: Option<String>,
    tool_use_id: Option<String>,
    name: Option<String>,
    summary: Option<String>,
    is_error: Option<bool>,
    content: Option<String>,
    commit: Option<String>,
    short_commit: Option<String>,
}

impl From<TurnEvent> for TurnEventPayload {
    fn from(event: TurnEvent) -> Self {
        let mut payload = Self {
            kind: "status",
            phase: None,
            label: None,
            text: None,
            elapsed_secs: None,
            step_commit: None,
            tool_use_id: None,
            name: None,
            summary: None,
            is_error: None,
            content: None,
            commit: None,
            short_commit: None,
        };
        match event {
            TurnEvent::PhaseStarted(phase) => {
                payload.kind = "phaseStarted";
                payload.phase = Some(phase_name(phase));
            }
            TurnEvent::PhaseComplete {
                label,
                elapsed_secs,
            } => {
                payload.kind = "phaseComplete";
                payload.label = Some(label);
                payload.elapsed_secs = Some(elapsed_secs);
            }
            TurnEvent::Status(text) => payload.text = Some(text),
            TurnEvent::AssistantText(text) => {
                payload.kind = "assistantText";
                payload.text = Some(text);
            }
            TurnEvent::ToolCall {
                step_commit,
                tool_use_id,
                name,
                summary,
            } => {
                payload.kind = "toolCall";
                payload.step_commit = Some(step_commit);
                payload.tool_use_id = Some(tool_use_id);
                payload.name = Some(name);
                payload.summary = Some(summary);
            }
            TurnEvent::ToolResult {
                step_commit,
                tool_use_id,
                is_error,
                content,
            } => {
                payload.kind = "toolResult";
                payload.step_commit = Some(step_commit);
                payload.tool_use_id = Some(tool_use_id);
                payload.is_error = Some(is_error);
                payload.content = Some(content);
            }
            TurnEvent::Completed(outcome) => {
                payload.kind = "completed";
                payload.commit = Some(outcome.commit);
                payload.short_commit = Some(outcome.short_commit);
            }
        }
        payload
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

fn automatic_title(prompt: &str) -> String {
    const MAX_CHARS: usize = 60;
    let title = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.chars().count() <= MAX_CHARS {
        return title;
    }
    title
        .chars()
        .take(MAX_CHARS - 1)
        .chain(std::iter::once('…'))
        .collect()
}

fn validated_conversation_title(title: &str) -> Result<String, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("conversation title cannot be empty".to_string());
    }
    if title.contains(['\n', '\r', '\t']) {
        return Err("conversation title must be one line".to_string());
    }
    Ok(title.to_string())
}

fn fresh_conversation_id(transport: &GitTransport, user: &str) -> Result<String, String> {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("reading the clock: {error}"))?
        .as_nanos();
    let descriptor = format!(
        "caos conversation v1\ncreator {user}\ncreated {created}\nprocess {}\n",
        std::process::id()
    );
    transport
        .put_object("blob", descriptor.as_bytes())
        .map(|id| id.to_string())
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
    let repo_path = repo_dir.display().to_string();
    let user = state.user.clone();
    run_blocking(move || {
        let transport = GitTransport::discover(&repo_dir)?;
        let conversations = active_conversations(&transport, &user)?;
        Ok(BootstrapPayload {
            repo_name,
            repo_path,
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
            return Ok(ConversationPayload {
                id,
                title,
                head: String::new(),
                short_head: String::new(),
                updated_unix: 0,
                draft: true,
                started: false,
            });
        }
        let transport = GitTransport::discover(repo_dir)?;
        let id = fresh_conversation_id(&transport, &user)?;
        drafts
            .lock()
            .map_err(|_| "desktop draft state is unavailable".to_string())?
            .insert(id.clone(), DraftState::default());
        Ok(ConversationPayload {
            id,
            title: "New conversation".to_string(),
            head: String::new(),
            short_head: String::new(),
            updated_unix: 0,
            draft: true,
            started: false,
        })
    })
    .await
}

#[tauri::command]
async fn rename_conversation(
    state: State<'_, AppState>,
    conversation: String,
    title: String,
) -> Result<String, String> {
    let title = validated_conversation_title(&title)?;
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
                    author: turn.author,
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
async fn get_diff(state: State<'_, AppState>, conversation: String) -> Result<DiffPayload, String> {
    if state
        .drafts
        .lock()
        .map_err(|_| "desktop draft state is unavailable".to_string())?
        .contains_key(&conversation)
    {
        return Ok(DiffPayload {
            base_commit: String::new(),
            head: String::new(),
            patch: String::new(),
        });
    }
    let repo_dir = state.repo_dir()?;
    run_blocking(move || {
        let transport = GitTransport::discover(repo_dir)?;
        conversation_workspace_diff(&transport, &conversation).map(|diff| DiffPayload {
            base_commit: diff.base_commit,
            head: diff.head,
            patch: diff.patch,
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
    on_event: Channel<TurnEventPayload>,
) -> Result<TurnOutcomePayload, String> {
    if message.trim().is_empty() {
        return Err("empty message".to_string());
    }
    {
        let mut active = state
            .active_turns
            .lock()
            .map_err(|_| "desktop turn state is unavailable".to_string())?;
        if !active.insert(conversation.clone()) {
            return Err(format!("conversation {conversation:?} is already running"));
        }
    }

    let repo_dir = state.repo_dir()?;
    let user = state.user.clone();
    let drafts = Arc::clone(&state.drafts);
    let active_turns = Arc::clone(&state.active_turns);
    let conversation_for_error = conversation.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = (|| {
            let draft_title = {
                let mut drafts = drafts
                    .lock()
                    .map_err(|_| "desktop draft state is unavailable".to_string())?;
                drafts.get_mut(&conversation).map(|draft| {
                    draft.started = true;
                    let title = draft
                        .title
                        .clone()
                        .unwrap_or_else(|| automatic_title(&message));
                    draft.title = Some(title.clone());
                    title
                })
            };
            let is_draft = draft_title.is_some();
            let resolved_title = draft_title.unwrap_or(title);
            let transport = GitTransport::discover(&repo_dir)?;
            let outcome = run_chat_turn(
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
            Ok(TurnOutcomePayload {
                conversation: outcome.conversation,
                commit: outcome.commit,
                short_commit: outcome.short_commit,
                title: resolved_title,
            })
        })();
        if let Ok(mut active) = active_turns.lock() {
            active.remove(&conversation_for_error);
        }
        result
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
    use super::{automatic_title, short_hash, validated_conversation_title};

    #[test]
    fn titles_match_the_tui_limits() {
        assert_eq!(
            automatic_title("  Review\t the\nparser  "),
            "Review the parser"
        );
        assert_eq!(
            automatic_title(&"x".repeat(61)),
            format!("{}…", "x".repeat(59))
        );
    }

    #[test]
    fn hashes_are_safe_when_short() {
        assert_eq!(short_hash("123456789"), "1234567");
        assert_eq!(short_hash("abc"), "abc");
    }

    #[test]
    fn conversation_titles_are_trimmed_and_single_line() {
        assert_eq!(
            validated_conversation_title("  A useful title  ").unwrap(),
            "A useful title"
        );
        assert!(validated_conversation_title("  ").is_err());
        assert!(validated_conversation_title("two\nlines").is_err());
        assert!(validated_conversation_title("tab\tseparated").is_err());
    }
}
