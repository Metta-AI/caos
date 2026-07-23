//! Presentation-independent boundary between the terminal UI and CAOS chat.
//!
//! The shape deliberately follows the useful parts of agent client protocols:
//! explicit capabilities, loadable session snapshots, and structured updates.
//! It is an internal Rust API, not a second wire protocol.

use std::path::{Path, PathBuf};

use caos::chat::{
    conversation_history, conversation_workspace_diff, describe_tool_set, list_conversations,
    run_chat_turn, ConversationRole, TurnEvent,
};
use caos::{GitTransport, Transport};

pub(crate) use caos::chat::{ConversationSummary, ToolSetDescription, TurnOptions, WorkspaceDiff};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BackendCapabilities {
    pub cancellation: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MessageRole {
    Human,
    Agent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConversationMessage {
    pub role: MessageRole,
    pub commit: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConversationSnapshot {
    pub messages: Vec<ConversationMessage>,
    pub diff: WorkspaceDiff,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TurnUpdate {
    PhaseComplete {
        label: String,
        elapsed_secs: f64,
    },
    Status(String),
    AssistantMessage(String),
    ToolCall {
        step_commit: String,
        id: String,
        name: String,
        summary: String,
    },
    ToolCallUpdate {
        step_commit: String,
        id: String,
        is_error: bool,
        content: String,
    },
    Completed {
        commit: String,
        short_commit: String,
    },
}

pub(crate) trait ChatBackend: Send + Sync {
    fn work_dir(&self) -> &Path;

    fn capabilities(&self) -> BackendCapabilities;

    fn resolve_revision(&self, revision: &str) -> Result<Option<String>, String>;

    fn list_conversations(&self) -> Result<Vec<ConversationSummary>, String>;

    fn load_conversation(&self, name: &str) -> Result<ConversationSnapshot, String>;

    fn describe_tools(
        &self,
        name: &str,
        options: &TurnOptions,
    ) -> Result<ToolSetDescription, String>;

    fn run_turn(
        &self,
        name: &str,
        prompt: &str,
        options: &TurnOptions,
        on_update: &mut dyn FnMut(TurnUpdate),
    ) -> Result<(), String>;
}

pub(crate) struct CaosBackend {
    work_dir: PathBuf,
}

impl CaosBackend {
    pub(crate) fn from_cwd() -> Result<Self, String> {
        let transport = GitTransport::from_cwd()?;
        Ok(Self {
            work_dir: transport.work_dir().to_path_buf(),
        })
    }

    fn transport(&self) -> Result<GitTransport, String> {
        GitTransport::discover(&self.work_dir)
    }
}

impl ChatBackend for CaosBackend {
    fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
    }

    fn resolve_revision(&self, revision: &str) -> Result<Option<String>, String> {
        self.transport()?
            .resolve_revspec(revision)
            .map(|commit| commit.map(|commit| commit.to_string()))
    }

    fn list_conversations(&self) -> Result<Vec<ConversationSummary>, String> {
        list_conversations(&self.transport()?)
    }

    fn load_conversation(&self, name: &str) -> Result<ConversationSnapshot, String> {
        let transport = self.transport()?;
        let messages = conversation_history(&transport, name)?
            .into_iter()
            .map(|turn| ConversationMessage {
                role: match turn.role {
                    ConversationRole::Human => MessageRole::Human,
                    ConversationRole::Agent => MessageRole::Agent,
                },
                commit: turn.commit,
                text: turn.message,
            })
            .collect();
        let diff = conversation_workspace_diff(&transport, name)?;
        Ok(ConversationSnapshot { messages, diff })
    }

    fn describe_tools(
        &self,
        name: &str,
        options: &TurnOptions,
    ) -> Result<ToolSetDescription, String> {
        describe_tool_set(&self.transport()?, name, options)
    }

    fn run_turn(
        &self,
        name: &str,
        prompt: &str,
        options: &TurnOptions,
        on_update: &mut dyn FnMut(TurnUpdate),
    ) -> Result<(), String> {
        run_chat_turn(&self.transport()?, options, name, prompt, |event| {
            on_update(event.into());
        })
        .map(|_| ())
    }
}

impl From<TurnEvent> for TurnUpdate {
    fn from(event: TurnEvent) -> Self {
        match event {
            TurnEvent::PhaseComplete {
                label,
                elapsed_secs,
            } => Self::PhaseComplete {
                label,
                elapsed_secs,
            },
            TurnEvent::Status(status) => Self::Status(status),
            TurnEvent::AssistantText(text) => Self::AssistantMessage(text),
            TurnEvent::ToolCall {
                step_commit,
                tool_use_id,
                name,
                summary,
            } => Self::ToolCall {
                step_commit,
                id: tool_use_id,
                name,
                summary,
            },
            TurnEvent::ToolResult {
                step_commit,
                tool_use_id,
                is_error,
                content,
            } => Self::ToolCallUpdate {
                step_commit,
                id: tool_use_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use caos::chat::TurnOutcome;

    #[test]
    fn harness_events_map_to_client_updates_without_losing_tool_identity() {
        assert_eq!(
            TurnUpdate::from(TurnEvent::ToolCall {
                step_commit: "step".to_string(),
                tool_use_id: "tool-1".to_string(),
                name: "build".to_string(),
                summary: "build workspace".to_string(),
            }),
            TurnUpdate::ToolCall {
                step_commit: "step".to_string(),
                id: "tool-1".to_string(),
                name: "build".to_string(),
                summary: "build workspace".to_string(),
            }
        );
        assert_eq!(
            TurnUpdate::from(TurnEvent::Completed(TurnOutcome {
                conversation: "talk-1".to_string(),
                commit: "commit".to_string(),
                short_commit: "short".to_string(),
            })),
            TurnUpdate::Completed {
                commit: "commit".to_string(),
                short_commit: "short".to_string(),
            }
        );
    }
}
