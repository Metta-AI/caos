//! Background lifecycle shared by computation jobs and child conversations.
//! A call is an occurrence; a task describes the work it launched.
use super::canonical::{canonical_bytes, parse_canonical};
use super::{AsyncRecord, AsyncStatus, ChildRecord, ChildStatus, ChildWorkspace, Oid};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// Keep the two durable records inline; there are few tasks per conversation.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "state",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TaskRecord {
    Computation(AsyncRecord),
    Conversation(ChildRecord),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Complete,
    Failed,
    Cancelled,
}

pub enum TaskOutcome {
    Computation {
        status: AsyncStatus,
        result: Option<Oid>,
        reason: Option<String>,
    },
    Conversation {
        status: ChildStatus,
        head: Oid,
        workspaces: BTreeMap<String, ChildWorkspace>,
    },
}

impl TaskRecord {
    pub fn computation(&self) -> &Oid {
        match self {
            Self::Computation(task) => &task.task,
            Self::Conversation(child) => &child.relay,
        }
    }
    pub fn status(&self) -> TaskStatus {
        match self {
            Self::Computation(task) => match task.status {
                AsyncStatus::Pending => TaskStatus::Pending,
                AsyncStatus::Complete => TaskStatus::Complete,
                AsyncStatus::Failed => TaskStatus::Failed,
                AsyncStatus::Cancelled => TaskStatus::Cancelled,
            },
            Self::Conversation(child) => match child.status {
                ChildStatus::Running => TaskStatus::Pending,
                ChildStatus::Completed => TaskStatus::Complete,
                ChildStatus::Failed => TaskStatus::Failed,
                ChildStatus::Cancelled => TaskStatus::Cancelled,
            },
        }
    }
    pub fn is_pending(&self) -> bool {
        self.status() == TaskStatus::Pending
    }
    pub fn finish(self, outcome: TaskOutcome) -> Result<Self, String> {
        if !self.is_pending() {
            return Err("task is already terminal".into());
        }
        let completed = match (self, outcome) {
            (
                Self::Computation(mut task),
                TaskOutcome::Computation {
                    status,
                    result,
                    reason,
                },
            ) => {
                task.status = status;
                task.result = result;
                task.reason = reason;
                Self::Computation(task)
            }
            (
                Self::Conversation(mut child),
                TaskOutcome::Conversation {
                    status,
                    head,
                    workspaces,
                },
            ) => {
                child.status = status;
                child.terminal_head = Some(head);
                child.child_workspaces = Some(workspaces);
                Self::Conversation(child)
            }
            _ => return Err("task outcome has the wrong variant".into()),
        };
        if completed.is_pending() {
            return Err("task outcome must be terminal".into());
        }
        completed.validate()?;
        Ok(completed)
    }
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Computation(task) => {
                AsyncRecord::from_value(&task.to_value())?;
            }
            Self::Conversation(child) => {
                ChildRecord::from_value(&child.to_value())?;
            }
        }
        Ok(())
    }
    pub fn encode(&self) -> Vec<u8> {
        canonical_bytes(&serde_json::to_value(self).expect("task serialization"))
            .expect("canonical task")
    }
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let task: Self =
            serde_json::from_value(parse_canonical(bytes)?).map_err(|e| e.to_string())?;
        task.validate()?;
        if task.encode() != bytes {
            return Err("noncanonical task".into());
        }
        Ok(task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_variants_share_terminal_guards_without_losing_their_records() {
        let mut store = super::super::tree::MemoryStore::new();
        let head = super::super::fixtures::golden(&mut store);
        let view = super::super::view::Conversation::open(&store, &head).unwrap();
        let tasks = view.tasks().unwrap();
        assert!(tasks
            .iter()
            .any(|task| matches!(task, TaskRecord::Computation(_))));
        assert!(tasks
            .iter()
            .any(|task| matches!(task, TaskRecord::Conversation(_))));
        for task in tasks {
            assert_eq!(TaskRecord::parse(&task.encode()).unwrap(), task);
            if !task.is_pending() {
                assert!(task
                    .clone()
                    .finish(TaskOutcome::Computation {
                        status: AsyncStatus::Cancelled,
                        result: None,
                        reason: None,
                    })
                    .unwrap_err()
                    .contains("already terminal"));
            }
            if let TaskRecord::Computation(mut record) = task {
                record.status = AsyncStatus::Pending;
                record.result = None;
                record.reason = None;
                let pending = TaskRecord::Computation(record);
                assert!(pending
                    .clone()
                    .finish(TaskOutcome::Conversation {
                        status: ChildStatus::Completed,
                        head: head.clone(),
                        workspaces: BTreeMap::new(),
                    })
                    .unwrap_err()
                    .contains("wrong variant"));
                assert_eq!(
                    pending
                        .finish(TaskOutcome::Computation {
                            status: AsyncStatus::Cancelled,
                            result: None,
                            reason: Some("cancelled".into()),
                        })
                        .unwrap()
                        .status(),
                    TaskStatus::Cancelled
                );
            }
        }
    }
}
