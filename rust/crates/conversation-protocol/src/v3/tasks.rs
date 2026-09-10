//! Background computations and conversations share one lifecycle.
use super::{AsyncRecord, ChildRecord, Oid};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Complete,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn finish(self, next: Self) -> Result<Self, String> {
        if self != Self::Pending {
            return Err("task is already terminal".into());
        }
        if next == Self::Pending {
            return Err("task outcome must be terminal".into());
        }
        Ok(next)
    }
}

// A view across the two task payloads, without another lifecycle or identity.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskRecord {
    Computation(AsyncRecord),
    Conversation(ChildRecord),
}

impl TaskRecord {
    pub fn computation(&self) -> &Oid {
        match self {
            Self::Computation(task) => &task.task,
            Self::Conversation(child) => &child.relay,
        }
    }
    pub fn is_pending(&self) -> bool {
        (match self {
            Self::Computation(task) => task.status,
            Self::Conversation(child) => child.status,
        }) == TaskStatus::Pending
    }
}
