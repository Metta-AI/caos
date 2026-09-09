use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[cfg(test)]
use super::canonical::canonical_object;
use super::canonical::{canonical_bytes, parse_canonical};
use super::oid::Oid;
use super::paths::{self, validate_tree_path, MAX_JSON_INT};

#[cfg(test)]
fn obj(fields: &[(&str, Value)]) -> Value {
    canonical_object(fields)
}

#[cfg(test)]
fn value_str(value: &str) -> Value {
    Value::String(value.to_string())
}

fn path(value: &str, what: &str) -> Result<(), String> {
    validate_tree_path(value)?;
    if !value.starts_with(".caos/") {
        return Err(format!("{what} must start with .caos/, got {value:?}"));
    }
    Ok(())
}

fn relative_path(value: &str, what: &str) -> Result<(), String> {
    validate_tree_path(value).map_err(|_| format!("invalid {what} {value:?}"))
}

fn encode_record(value: Value) -> Vec<u8> {
    canonical_bytes(&value).expect("record values are canonical JSON objects")
}

fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

fn some_of<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer).map(Some)
}

fn absent<T>(value: &Option<T>) -> bool {
    value.is_none()
}

mod arguments_path {
    use super::*;

    #[derive(Serialize)]
    struct Arguments<'a> {
        path: &'a str,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OwnedArguments {
        path: String,
    }

    pub fn serialize<S>(value: &String, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Arguments { path: value }.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        OwnedArguments::deserialize(deserializer).map(|arguments| arguments.path)
    }
}

pub(crate) trait Record: Sized {
    fn to_value(&self) -> Value;
    fn from_value(value: &Value) -> Result<Self, String>;
    fn parse_record(bytes: &[u8]) -> Result<Self, String>;
}

macro_rules! impl_record {
    ($type:ty $(, $validate:ident)?) => {
        impl $type {
            pub fn to_value(&self) -> Value {
                serde_json::to_value(self).expect("record serialization is infallible")
            }

            pub fn from_value(value: &Value) -> Result<Self, String> {
                let record: Self =
                    serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
                $(record.$validate()?;)?
                Ok(record)
            }

            pub fn encode(&self) -> Vec<u8> {
                encode_record(self.to_value())
            }

            pub fn parse(bytes: &[u8]) -> Result<Self, String> {
                Self::from_value(&parse_canonical(bytes)?)
            }
        }

        impl Record for $type {
            fn to_value(&self) -> Value {
                <$type>::to_value(self)
            }

            fn from_value(value: &Value) -> Result<Self, String> {
                <$type>::from_value(value)
            }

            fn parse_record(bytes: &[u8]) -> Result<Self, String> {
                Self::parse(bytes)
            }
        }
    };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityKind {
    Root,
    Fork { source: Oid },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub parent: String,
    pub parent_head: Oid,
    pub request: Oid,
    pub round: u64,
    pub tool: String,
}

impl_record!(Owner, validate);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawIdentity", into = "RawIdentity")]
pub struct Identity {
    pub id: String,
    pub kind: IdentityKind,
    pub owner: Option<Owner>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIdentity {
    id: String,
    kind: String,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    source: Option<Oid>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    owner: Option<Owner>,
}

impl TryFrom<RawIdentity> for Identity {
    type Error = String;

    fn try_from(raw: RawIdentity) -> Result<Self, Self::Error> {
        let kind = match (raw.kind.as_str(), raw.source) {
            ("root", None) => IdentityKind::Root,
            ("root", Some(_)) => return Err("identity source is forbidden for root".to_string()),
            ("fork", Some(source)) => IdentityKind::Fork { source },
            ("fork", None) => return Err("identity source is required for fork".to_string()),
            (kind, _) => return Err(format!("invalid identity kind {kind:?}")),
        };
        if raw.owner.is_some() && !matches!(kind, IdentityKind::Root) {
            return Err("identity owner is only allowed for root".to_string());
        }
        Ok(Identity {
            id: raw.id,
            kind,
            owner: raw.owner,
        })
    }
}

impl From<Identity> for RawIdentity {
    fn from(identity: Identity) -> Self {
        let (kind, source) = match identity.kind {
            IdentityKind::Root => ("root".to_string(), None),
            IdentityKind::Fork { source } => ("fork".to_string(), Some(source)),
        };
        RawIdentity {
            id: identity.id,
            kind,
            source,
            owner: identity.owner,
        }
    }
}

impl_record!(Identity, validate);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceOrigin {
    pub source: String,
    pub source_tree: Oid,
}

impl_record!(WorkspaceOrigin);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRecord {
    pub commit: Oid,
    pub initial: Oid,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub origin: Option<WorkspaceOrigin>,
}

impl_record!(WorkspaceRecord);

pub fn encode_title(title: &str) -> Result<Vec<u8>, String> {
    if title.is_empty()
        || title.len() > paths::MAX_TITLE
        || title.bytes().any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
    {
        return Err(format!("invalid conversation title {title:?}"));
    }
    Ok(format!("{title}\n").into_bytes())
}

pub fn parse_title(bytes: &[u8]) -> Result<String, String> {
    let title = bytes
        .strip_suffix(b"\n")
        .ok_or_else(|| "conversation title must end with LF".to_string())?;
    let title = std::str::from_utf8(title)
        .map_err(|_| "conversation title must be UTF-8".to_string())?
        .to_string();
    if encode_title(&title)? != bytes {
        return Err("invalid conversation title encoding".to_string());
    }
    Ok(title)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Block {
    Text {
        text: String,
    },
    Payload {
        path: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(with = "arguments_path")]
        arguments: String,
    },
}

impl_record!(Block, validate);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub base: Oid,
    pub commit: Oid,
    pub workspace_name: String,
}

impl_record!(Proposal, validate);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeInfo {
    pub base: Oid,
    pub ours: Oid,
    pub theirs: Oid,
    pub implementation: String,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub output: Option<Oid>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub conflict_paths: Option<Vec<String>>,
}

impl_record!(MergeInfo, validate);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceResolution {
    AlreadyApplied {
        current: Oid,
        #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
        candidate: Option<Oid>,
    },
    Direct {
        current: Oid,
        output: Oid,
    },
    Merged {
        current: Oid,
        merge: MergeInfo,
        output: Oid,
    },
    Conflict {
        #[serde(deserialize_with = "nullable")]
        current: Option<Oid>,
        candidate: Oid,
        #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
        merge: Option<MergeInfo>,
    },
}

impl WorkspaceResolution {
    pub fn new_pointer(&self) -> Option<&Oid> {
        match self {
            WorkspaceResolution::AlreadyApplied { .. } | WorkspaceResolution::Conflict { .. } => {
                None
            }
            WorkspaceResolution::Direct { output, .. }
            | WorkspaceResolution::Merged { output, .. } => Some(output),
        }
    }

    pub fn candidate(&self) -> Option<&Oid> {
        match self {
            WorkspaceResolution::AlreadyApplied { candidate, .. } => candidate.as_ref(),
            WorkspaceResolution::Direct { output, .. } => Some(output),
            WorkspaceResolution::Merged { merge, .. } => Some(&merge.theirs),
            WorkspaceResolution::Conflict { candidate, .. } => Some(candidate),
        }
    }
}

impl_record!(WorkspaceResolution, validate);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptEntry {
    pub message_id: String,
    pub conversation: String,
    pub role: Role,
    pub actor: String,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub request: Option<Oid>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub round: Option<u64>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub model: Option<String>,
    pub blocks: Vec<Block>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub proposal: Option<Proposal>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub workspace_resolution: Option<WorkspaceResolution>,
}

impl_record!(TranscriptEntry, validate);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredCall {
    pub id: String,
    pub name: String,
}

impl_record!(DeclaredCall);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestStatus {
    Queued,
    Running,
    Cancelling,
    Idle,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum RequestOutcome {
    Idle {
        result: Option<String>,
        interrupted: bool,
    },
    Failed {
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawRequestRecord", into = "RawRequestRecord")]
pub struct RequestRecord {
    pub id: Oid,
    pub request_head: Oid,
    pub request_workspaces: Option<Oid>,
    pub model: String,
    pub configuration: String,
    pub round: u64,
    pub calls: Vec<DeclaredCall>,
    pub interjections: Vec<String>,
    pub status: RequestStatus,
    pub latest_message: Option<String>,
    pub escape_reason: Option<String>,
    pub outcome: Option<RequestOutcome>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequestRecord {
    id: Oid,
    request_head: Oid,
    #[serde(deserialize_with = "nullable")]
    request_workspaces: Option<Oid>,
    model: String,
    configuration: String,
    round: u64,
    calls: Vec<DeclaredCall>,
    interjections: Vec<String>,
    status: RequestStatus,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    latest_message: Option<String>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    escape_reason: Option<String>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "absent"
    )]
    result: Option<Option<String>>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    interrupted: Option<bool>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    error: Option<String>,
}

impl TryFrom<RawRequestRecord> for RequestRecord {
    type Error = String;

    fn try_from(raw: RawRequestRecord) -> Result<Self, Self::Error> {
        let outcome = match raw.status {
            RequestStatus::Idle => {
                if raw.error.is_some() {
                    return Err("request error is forbidden for idle status".to_string());
                }
                Some(RequestOutcome::Idle {
                    result: raw
                        .result
                        .ok_or_else(|| "request result is required for idle status".to_string())?,
                    interrupted: raw.interrupted.ok_or_else(|| {
                        "request interrupted is required for idle status".to_string()
                    })?,
                })
            }
            RequestStatus::Failed => {
                if raw.result.is_some() || raw.interrupted.is_some() {
                    return Err("request idle outcome is forbidden for failed status".to_string());
                }
                Some(RequestOutcome::Failed {
                    error: raw
                        .error
                        .ok_or_else(|| "request error is required for failed status".to_string())?,
                })
            }
            _ => {
                if raw.result.is_some() || raw.interrupted.is_some() || raw.error.is_some() {
                    return Err("request outcome is forbidden for nonterminal status".to_string());
                }
                None
            }
        };
        let record = RequestRecord {
            id: raw.id,
            request_head: raw.request_head,
            request_workspaces: raw.request_workspaces,
            model: raw.model,
            configuration: raw.configuration,
            round: raw.round,
            calls: raw.calls,
            interjections: raw.interjections,
            status: raw.status,
            latest_message: raw.latest_message,
            escape_reason: raw.escape_reason,
            outcome,
        };
        record.validate()?;
        Ok(record)
    }
}

impl From<RequestRecord> for RawRequestRecord {
    fn from(record: RequestRecord) -> Self {
        let (result, interrupted, error) = match record.outcome {
            Some(RequestOutcome::Idle {
                result,
                interrupted,
            }) => (Some(result), Some(interrupted), None),
            Some(RequestOutcome::Failed { error }) => (None, None, Some(error)),
            None => (None, None, None),
        };
        RawRequestRecord {
            id: record.id,
            request_head: record.request_head,
            request_workspaces: record.request_workspaces,
            model: record.model,
            configuration: record.configuration,
            round: record.round,
            calls: record.calls,
            interjections: record.interjections,
            status: record.status,
            latest_message: record.latest_message,
            escape_reason: record.escape_reason,
            result,
            interrupted,
            error,
        }
    }
}

impl RequestRecord {
    fn validate(&self) -> Result<(), String> {
        if self.round > MAX_JSON_INT {
            return Err("request round exceeds the maximum JSON integer".to_string());
        }
        if self.configuration.is_empty() {
            return Err("request configuration must not be empty".to_string());
        }
        if matches!(
            self.status,
            RequestStatus::Running | RequestStatus::Cancelling
        ) != self.latest_message.is_some()
        {
            return Err(
                "request latest_message is required only for running or cancelling status"
                    .to_string(),
            );
        }
        if self.escape_reason.is_some()
            && !matches!(self.status, RequestStatus::Cancelling | RequestStatus::Idle)
        {
            return Err(
                "request escape_reason is allowed only for cancelling or idle status".to_string(),
            );
        }
        match (&self.status, &self.outcome) {
            (RequestStatus::Idle, Some(RequestOutcome::Idle { result, .. })) => {
                if let Some(result) = result {
                    path(result, "request result path")?;
                }
            }
            (RequestStatus::Failed, Some(RequestOutcome::Failed { error })) => {
                path(error, "request error path")?;
            }
            (RequestStatus::Idle, _) => {
                return Err("request idle status requires idle outcome".to_string())
            }
            (RequestStatus::Failed, _) => {
                return Err("request failed status requires failed outcome".to_string())
            }
            (_, None) => {}
            _ => return Err("request outcome is forbidden for nonterminal status".to_string()),
        }
        Ok(())
    }
}

impl_record!(RequestRecord);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolStatus {
    Started,
    Complete,
    Failed,
    Cancelled,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum ToolResult {
    Complete {
        observation: String,
        #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
        proposal: Option<Oid>,
    },
    Failed {
        error: String,
    },
    Cancelled {
        reason: String,
    },
}

impl_record!(ToolResult, validate);

impl ToolResult {
    pub fn observation(&self) -> Option<&str> {
        match self {
            ToolResult::Complete { observation, .. } => Some(observation),
            ToolResult::Failed { .. } | ToolResult::Cancelled { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesOutcome {
    pub applied: Vec<String>,
    pub conflicted: Vec<String>,
}

impl_record!(FilesOutcome, validate);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawToolRecord", into = "RawToolRecord")]
pub struct ToolRecord {
    pub request: Oid,
    pub round: u64,
    pub id: String,
    pub name: String,
    pub declaration_message: String,
    pub workspace_name: Option<String>,
    pub input_workspace: Option<Oid>,
    pub status: ToolStatus,
    pub task: Option<Oid>,
    pub result: Option<ToolResult>,
    pub workspace_resolution: Option<WorkspaceResolution>,
    pub files: Vec<String>,
    pub files_outcome: Option<FilesOutcome>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToolRecord {
    request: Oid,
    round: u64,
    id: String,
    name: String,
    declaration_message: String,
    #[serde(deserialize_with = "nullable")]
    workspace_name: Option<String>,
    #[serde(deserialize_with = "nullable")]
    input_workspace: Option<Oid>,
    status: ToolStatus,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    task: Option<Oid>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    result: Option<ToolResult>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    workspace_resolution: Option<WorkspaceResolution>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    files: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "absent"
    )]
    files_outcome: Option<Option<FilesOutcome>>,
}

impl TryFrom<RawToolRecord> for ToolRecord {
    type Error = String;

    fn try_from(raw: RawToolRecord) -> Result<Self, Self::Error> {
        let (files, files_outcome) = if raw.status == ToolStatus::Started {
            if raw.task.is_none()
                || raw.result.is_some()
                || raw.workspace_resolution.is_some()
                || raw.files.is_some()
                || raw.files_outcome.is_some()
            {
                return Err("started tool carries invalid fields".to_string());
            }
            (Vec::new(), None)
        } else {
            if raw.result.is_none() {
                return Err("terminal tool requires result".to_string());
            }
            (
                raw.files
                    .ok_or_else(|| "terminal tool requires files".to_string())?,
                raw.files_outcome
                    .ok_or_else(|| "terminal tool requires files_outcome".to_string())?,
            )
        };
        let record = ToolRecord {
            request: raw.request,
            round: raw.round,
            id: raw.id,
            name: raw.name,
            declaration_message: raw.declaration_message,
            workspace_name: raw.workspace_name,
            input_workspace: raw.input_workspace,
            status: raw.status,
            task: raw.task,
            result: raw.result,
            workspace_resolution: raw.workspace_resolution,
            files,
            files_outcome,
        };
        record.validate()?;
        Ok(record)
    }
}

impl From<ToolRecord> for RawToolRecord {
    fn from(record: ToolRecord) -> Self {
        let terminal = record.status != ToolStatus::Started;
        RawToolRecord {
            request: record.request,
            round: record.round,
            id: record.id,
            name: record.name,
            declaration_message: record.declaration_message,
            workspace_name: record.workspace_name,
            input_workspace: record.input_workspace,
            status: record.status,
            task: record.task,
            result: record.result,
            workspace_resolution: record.workspace_resolution,
            files: terminal.then_some(record.files),
            files_outcome: terminal.then_some(record.files_outcome),
        }
    }
}

impl ToolRecord {
    fn validate(&self) -> Result<(), String> {
        if self.round > MAX_JSON_INT {
            return Err("tool round exceeds the maximum JSON integer".to_string());
        }
        if let Some(workspace_name) = &self.workspace_name {
            paths::validate_workspace_name(workspace_name)?;
        }
        if self.workspace_name.is_some() != self.input_workspace.is_some() {
            return Err(
                "tool workspace_name and input_workspace must both be null or present".to_string(),
            );
        }
        if self.workspace_resolution.is_some() && self.workspace_name.is_none() {
            return Err("tool workspace_resolution requires workspace_name".to_string());
        }
        if self.status == ToolStatus::Started {
            if self.task.is_none()
                || self.result.is_some()
                || self.workspace_resolution.is_some()
                || !self.files.is_empty()
                || self.files_outcome.is_some()
            {
                return Err("started tool carries terminal fields".to_string());
            }
        } else {
            let result = self
                .result
                .as_ref()
                .ok_or_else(|| "terminal tool requires result".to_string())?;
            if Self::expected_status(result, self.workspace_resolution.as_ref()) != self.status {
                return Err("tool status does not match result".to_string());
            }
        }
        match (&self.result, &self.workspace_resolution) {
            (
                Some(ToolResult::Complete {
                    proposal: Some(_), ..
                }),
                Some(_),
            )
            | (Some(ToolResult::Complete { .. }), None)
            | (None, None) => {}
            (Some(ToolResult::Complete { proposal: None, .. }), Some(_)) => {
                return Err("tool workspace_resolution requires proposal".to_string())
            }
            (_, Some(_)) => {
                return Err("tool workspace_resolution requires complete result".to_string())
            }
            _ => {}
        }
        if let Some(result) = &self.result {
            result.validate()?;
        }
        if let Some(resolution) = &self.workspace_resolution {
            resolution.validate()?;
        }
        if let Some(outcome) = &self.files_outcome {
            outcome.validate()?;
        }
        for item in &self.files {
            relative_path(item, "tool file path")?;
        }
        Ok(())
    }
}

impl ToolRecord {
    pub fn is_terminal(&self) -> bool {
        self.status != ToolStatus::Started
    }

    pub fn expected_status(
        result: &ToolResult,
        resolution: Option<&WorkspaceResolution>,
    ) -> ToolStatus {
        match result {
            ToolResult::Complete { .. }
                if matches!(resolution, Some(WorkspaceResolution::Conflict { .. })) =>
            {
                ToolStatus::Conflict
            }
            ToolResult::Complete { .. } => ToolStatus::Complete,
            ToolResult::Failed { .. } => ToolStatus::Failed,
            ToolResult::Cancelled { .. } => ToolStatus::Cancelled,
        }
    }
}

impl_record!(ToolRecord);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AsyncStatus {
    Pending,
    Complete,
    Failed,
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsyncRecord {
    pub task: Oid,
    pub status: AsyncStatus,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub target_ref: Option<String>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub result: Option<Oid>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub reason: Option<String>,
}
impl_record!(AsyncRecord, validate);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnIntent {
    pub request: Oid,
    pub round: u64,
    pub tool: String,
    #[serde(deserialize_with = "nullable")]
    pub workspace_name: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub input_workspace: Option<Oid>,
    pub prompt: String,
    pub model: String,
    pub configuration: String,
    #[serde(deserialize_with = "nullable")]
    pub files_seed: Option<Oid>,
}
impl_record!(SpawnIntent, validate);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChildStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildWorkspace {
    pub commit: Oid,
    pub initial: Oid,
}
impl_record!(ChildWorkspace);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Application {
    pub parent_workspace_name: String,
    #[serde(deserialize_with = "nullable")]
    pub parent_workspace: Option<Oid>,
    pub child_workspace: String,
    pub workspace_resolution: WorkspaceResolution,
}
impl_record!(Application, validate);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildRecord {
    pub id: String,
    pub initial_head: Oid,
    #[serde(deserialize_with = "nullable")]
    pub initial_workspace: Option<Oid>,
    pub request: Oid,
    pub relay: Oid,
    pub spawn_intent: SpawnIntent,
    pub status: ChildStatus,
    pub applications: Vec<Application>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub terminal_head: Option<Oid>,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub child_workspaces: Option<BTreeMap<String, ChildWorkspace>>,
}
impl_record!(ChildRecord, validate);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub source_base: Oid,
    pub source_head: Oid,
    pub target_base: Oid,
    pub policy: String,
    pub implementation: String,
    pub commit_policy: String,
}
impl_record!(Descriptor, validate);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublicationStatus {
    Pending,
    Complete,
    Conflict,
    Uncertain,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub kind: String,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    pub diagnostic: Option<String>,
}
impl_record!(Evidence, validate);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawPublicationRecord", into = "RawPublicationRecord")]
pub struct PublicationRecord {
    pub id: String,
    pub key: String,
    pub descriptor: Descriptor,
    pub planned_head: Oid,
    pub repository: String,
    pub refname: String,
    pub expected_old: Option<Oid>,
    pub workspace_name: String,
    pub status: PublicationStatus,
    pub evidence: Option<Evidence>,
    pub observed: Option<Oid>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPublicationRecord {
    id: String,
    key: String,
    descriptor: Descriptor,
    planned_head: Oid,
    repository: String,
    #[serde(rename = "ref")]
    refname: String,
    #[serde(deserialize_with = "nullable")]
    expected_old: Option<Oid>,
    workspace_name: String,
    status: PublicationStatus,
    #[serde(default, deserialize_with = "some_of", skip_serializing_if = "absent")]
    evidence: Option<Evidence>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "absent"
    )]
    observed: Option<Option<Oid>>,
}

impl TryFrom<RawPublicationRecord> for PublicationRecord {
    type Error = String;

    fn try_from(raw: RawPublicationRecord) -> Result<Self, Self::Error> {
        let observed = if raw.status == PublicationStatus::Pending {
            if raw.evidence.is_some() || raw.observed.is_some() {
                return Err("pending publication forbids terminal fields".to_string());
            }
            None
        } else {
            if raw.evidence.is_none() || raw.observed.is_none() {
                return Err(
                    "publication evidence is required iff status is not pending".to_string()
                );
            }
            raw.observed
                .expect("terminal observation presence was checked")
        };
        let record = PublicationRecord {
            id: raw.id,
            key: raw.key,
            descriptor: raw.descriptor,
            planned_head: raw.planned_head,
            repository: raw.repository,
            refname: raw.refname,
            expected_old: raw.expected_old,
            workspace_name: raw.workspace_name,
            status: raw.status,
            evidence: raw.evidence,
            observed,
        };
        record.validate()?;
        Ok(record)
    }
}

impl From<PublicationRecord> for RawPublicationRecord {
    fn from(record: PublicationRecord) -> Self {
        let terminal = record.status != PublicationStatus::Pending;
        RawPublicationRecord {
            id: record.id,
            key: record.key,
            descriptor: record.descriptor,
            planned_head: record.planned_head,
            repository: record.repository,
            refname: record.refname,
            expected_old: record.expected_old,
            workspace_name: record.workspace_name,
            status: record.status,
            evidence: record.evidence,
            observed: terminal.then_some(record.observed),
        }
    }
}

impl PublicationRecord {
    fn validate(&self) -> Result<(), String> {
        paths::validate_workspace_name(&self.workspace_name)?;
        self.descriptor.validate()?;
        match self.status {
            PublicationStatus::Pending if self.evidence.is_none() && self.observed.is_none() => {}
            PublicationStatus::Pending => {
                return Err("pending publication forbids terminal fields".to_string())
            }
            _ if self.evidence.is_some() => {}
            _ => {
                return Err("publication evidence is required iff status is not pending".to_string())
            }
        }
        if let Some(evidence) = &self.evidence {
            evidence.validate()?;
        }
        Ok(())
    }
}
impl_record!(PublicationRecord);

impl Owner {
    fn validate(&self) -> Result<(), String> {
        if self.round > MAX_JSON_INT {
            return Err("owner round exceeds the maximum JSON integer".to_string());
        }
        Ok(())
    }
}

impl Identity {
    fn validate(&self) -> Result<(), String> {
        if self.owner.is_some() && !matches!(self.kind, IdentityKind::Root) {
            return Err("identity owner is only allowed for root".to_string());
        }
        if let Some(owner) = &self.owner {
            owner.validate()?;
        }
        Ok(())
    }
}

impl Block {
    fn validate(&self) -> Result<(), String> {
        match self {
            Block::Text { .. } => Ok(()),
            Block::Payload { path: value } => path(value, "payload path"),
            Block::ToolUse {
                arguments: value, ..
            } => path(value, "tool arguments path"),
        }
    }
}

impl Proposal {
    fn validate(&self) -> Result<(), String> {
        paths::validate_workspace_name(&self.workspace_name)
    }
}

impl MergeInfo {
    fn validate(&self) -> Result<(), String> {
        if self.output.is_some() == self.conflict_paths.is_some() {
            return Err("merge requires exactly one of output or conflict_paths".to_string());
        }
        if let Some(paths) = &self.conflict_paths {
            for item in paths {
                relative_path(item, "merge conflict path")?;
            }
        }
        Ok(())
    }
}

impl WorkspaceResolution {
    fn validate(&self) -> Result<(), String> {
        match self {
            WorkspaceResolution::Merged { merge, .. }
            | WorkspaceResolution::Conflict {
                merge: Some(merge), ..
            } => merge.validate(),
            _ => Ok(()),
        }
    }
}

impl TranscriptEntry {
    fn validate(&self) -> Result<(), String> {
        if self.round.is_some_and(|round| round > MAX_JSON_INT) {
            return Err("transcript round exceeds the maximum JSON integer".to_string());
        }
        for block in &self.blocks {
            block.validate()?;
        }
        if let Some(proposal) = &self.proposal {
            proposal.validate()?;
        }
        if let Some(resolution) = &self.workspace_resolution {
            resolution.validate()?;
        }
        Ok(())
    }
}

impl ToolResult {
    fn validate(&self) -> Result<(), String> {
        match self {
            ToolResult::Complete { observation, .. } => path(observation, "tool observation path"),
            ToolResult::Failed { error } => path(error, "tool error path"),
            ToolResult::Cancelled { .. } => Ok(()),
        }
    }
}

impl FilesOutcome {
    fn validate(&self) -> Result<(), String> {
        for item in self.applied.iter().chain(&self.conflicted) {
            relative_path(item, "files outcome path")?;
        }
        Ok(())
    }
}

impl AsyncRecord {
    fn validate(&self) -> Result<(), String> {
        let valid = match self.status {
            AsyncStatus::Pending => self.result.is_none() && self.reason.is_none(),
            AsyncStatus::Complete | AsyncStatus::Failed => {
                self.result.is_some() && self.reason.is_none()
            }
            AsyncStatus::Cancelled => self.result.is_none() && self.reason.is_some(),
        };
        if !valid {
            return Err("async terminal fields do not match status".to_string());
        }
        Ok(())
    }
}

impl SpawnIntent {
    fn validate(&self) -> Result<(), String> {
        if self.round > MAX_JSON_INT {
            return Err("spawn intent round exceeds the maximum JSON integer".to_string());
        }
        if self.workspace_name.is_some() != self.input_workspace.is_some() {
            return Err(
                "spawn intent workspace_name and input_workspace must both be null or present"
                    .to_string(),
            );
        }
        if let Some(workspace_name) = &self.workspace_name {
            paths::validate_workspace_name(workspace_name)?;
        }
        path(&self.prompt, "spawn prompt path")?;
        if self.configuration.is_empty() {
            return Err("spawn configuration must not be empty".to_string());
        }
        Ok(())
    }
}

impl Application {
    fn validate(&self) -> Result<(), String> {
        paths::validate_workspace_name(&self.parent_workspace_name)?;
        paths::validate_workspace_name(&self.child_workspace)?;
        self.workspace_resolution.validate()
    }
}

impl ChildRecord {
    fn validate(&self) -> Result<(), String> {
        if self.status == ChildStatus::Running {
            if self.terminal_head.is_some() || self.child_workspaces.is_some() {
                return Err("running child forbids terminal fields".to_string());
            }
        } else if self.terminal_head.is_none() || self.child_workspaces.is_none() {
            return Err("child terminal fields are required iff status is not running".to_string());
        }
        self.spawn_intent.validate()?;
        for application in &self.applications {
            application.validate()?;
        }
        if let Some(workspaces) = &self.child_workspaces {
            for name in workspaces.keys() {
                paths::validate_workspace_name(name)?;
            }
        }
        Ok(())
    }
}

impl Descriptor {
    fn validate(&self) -> Result<(), String> {
        if !matches!(self.policy.as_str(), "squash" | "preserve") {
            return Err(format!("invalid descriptor policy {:?}", self.policy));
        }
        Ok(())
    }
}

impl Evidence {
    fn validate(&self) -> Result<(), String> {
        if !matches!(
            self.kind.as_str(),
            "push-success"
                | "ref-converged"
                | "ref-drift"
                | "lease-rejected"
                | "ambiguous"
                | "operator-resolution"
        ) {
            return Err(format!("invalid evidence kind {:?}", self.kind));
        }
        Ok(())
    }
}

pub fn parse_active_request(bytes: &[u8]) -> Result<Oid, String> {
    Oid::parse_line(bytes, "workspace sha")
        .map_err(|error| format!("invalid active request: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(character: char) -> Oid {
        Oid::parse(&character.to_string().repeat(40), "test oid").unwrap()
    }

    fn direct() -> WorkspaceResolution {
        WorkspaceResolution::Direct {
            current: oid('a'),
            output: oid('b'),
        }
    }

    fn spawn_intent() -> SpawnIntent {
        SpawnIntent {
            request: oid('1'),
            round: 2,
            tool: "tool-1".to_string(),
            workspace_name: Some("main".to_string()),
            input_workspace: Some(oid('a')),
            prompt: ".caos/tools/prompt".to_string(),
            model: "model".to_string(),
            configuration: "configuration-hash".to_string(),
            files_seed: Some(oid('b')),
        }
    }

    fn child(status: ChildStatus) -> ChildRecord {
        let terminal = status != ChildStatus::Running;
        ChildRecord {
            id: "child-1".to_string(),
            initial_head: oid('2'),
            initial_workspace: Some(oid('a')),
            request: oid('1'),
            relay: oid('3'),
            spawn_intent: spawn_intent(),
            status,
            applications: vec![Application {
                parent_workspace_name: "main".to_string(),
                parent_workspace: Some(oid('a')),
                child_workspace: "main".to_string(),
                workspace_resolution: direct(),
            }],
            terminal_head: terminal.then(|| oid('4')),
            child_workspaces: terminal.then(|| {
                BTreeMap::from([(
                    "main".to_string(),
                    ChildWorkspace {
                        commit: oid('b'),
                        initial: oid('a'),
                    },
                )])
            }),
        }
    }

    fn descriptor() -> Descriptor {
        Descriptor {
            source_base: oid('a'),
            source_head: oid('b'),
            target_base: oid('c'),
            policy: "squash".to_string(),
            implementation: "project-v1".to_string(),
            commit_policy: "single".to_string(),
        }
    }

    #[test]
    fn record_bytes_are_stable() {
        let owner = Owner {
            parent: "parent".to_string(),
            parent_head: oid('a'),
            request: oid('b'),
            round: 3,
            tool: "tool".to_string(),
        };
        let origin = WorkspaceOrigin {
            source: "repo".to_string(),
            source_tree: oid('d'),
        };
        let merged = MergeInfo {
            base: oid('a'),
            ours: oid('b'),
            theirs: oid('c'),
            implementation: "merge-v1".to_string(),
            output: Some(oid('d')),
            conflict_paths: None,
        };
        let request = RequestRecord {
            id: oid('1'),
            request_head: oid('2'),
            request_workspaces: None,
            model: "model".to_string(),
            configuration: "configuration-hash".to_string(),
            round: 0,
            calls: vec![DeclaredCall {
                id: "tool-1".to_string(),
                name: "shell".to_string(),
            }],
            interjections: vec!["note".to_string()],
            status: RequestStatus::Idle,
            latest_message: None,
            escape_reason: Some("stop".to_string()),
            outcome: Some(RequestOutcome::Idle {
                result: Some(".caos/requests/result".to_string()),
                interrupted: true,
            }),
        };
        let tool_result = ToolResult::Complete {
            observation: ".caos/tools/observation".to_string(),
            proposal: Some(oid('b')),
        };
        let files_outcome = FilesOutcome {
            applied: vec!["a.txt".to_string()],
            conflicted: vec!["b.txt".to_string()],
        };
        let tool = ToolRecord {
            request: oid('1'),
            round: 0,
            id: "tool-1".to_string(),
            name: "shell".to_string(),
            declaration_message: "message-1".to_string(),
            workspace_name: Some("main".to_string()),
            input_workspace: Some(oid('a')),
            status: ToolStatus::Complete,
            task: None,
            result: Some(tool_result.clone()),
            workspace_resolution: Some(direct()),
            files: vec!["a.txt".to_string()],
            files_outcome: Some(files_outcome.clone()),
        };
        let application = Application {
            parent_workspace_name: "main".to_string(),
            parent_workspace: Some(oid('a')),
            child_workspace: "main".to_string(),
            workspace_resolution: direct(),
        };
        let transcript = TranscriptEntry {
            message_id: "message-1".to_string(),
            conversation: "conversation".to_string(),
            role: Role::Assistant,
            actor: "model".to_string(),
            request: Some(oid('1')),
            round: Some(0),
            model: Some("model".to_string()),
            blocks: vec![Block::ToolUse {
                id: "tool-1".to_string(),
                name: "shell".to_string(),
                arguments: ".caos/transcript/arguments".to_string(),
            }],
            proposal: Some(Proposal {
                base: oid('a'),
                commit: oid('b'),
                workspace_name: "main".to_string(),
            }),
            workspace_resolution: Some(direct()),
        };
        let publication = PublicationRecord {
            id: "publication-1".to_string(),
            key: "key".to_string(),
            descriptor: descriptor(),
            planned_head: oid('d'),
            repository: "repo".to_string(),
            refname: "refs/heads/main".to_string(),
            expected_old: None,
            workspace_name: "main".to_string(),
            status: PublicationStatus::Complete,
            evidence: Some(Evidence {
                kind: "push-success".to_string(),
                diagnostic: Some("ok".to_string()),
            }),
            observed: Some(oid('e')),
        };

        let encoded = [
            owner.clone().encode(),
            Identity {
                id: "root".to_string(),
                kind: IdentityKind::Root,
                owner: Some(owner),
            }
            .encode(),
            origin.clone().encode(),
            WorkspaceRecord {
                commit: oid('a'),
                initial: oid('b'),
                origin: Some(origin),
            }
            .encode(),
            Block::ToolUse {
                id: "tool-1".to_string(),
                name: "shell".to_string(),
                arguments: ".caos/transcript/arguments".to_string(),
            }
            .encode(),
            Proposal {
                base: oid('a'),
                commit: oid('b'),
                workspace_name: "main".to_string(),
            }
            .encode(),
            merged.clone().encode(),
            WorkspaceResolution::Merged {
                current: oid('b'),
                merge: merged,
                output: oid('d'),
            }
            .encode(),
            transcript.encode(),
            DeclaredCall {
                id: "tool-1".to_string(),
                name: "shell".to_string(),
            }
            .encode(),
            request.encode(),
            tool_result.encode(),
            files_outcome.encode(),
            tool.encode(),
            AsyncRecord {
                task: oid('1'),
                status: AsyncStatus::Complete,
                target_ref: Some("refs/heads/main".to_string()),
                result: Some(oid('2')),
                reason: None,
            }
            .encode(),
            spawn_intent().encode(),
            ChildWorkspace {
                commit: oid('b'),
                initial: oid('a'),
            }
            .encode(),
            application.encode(),
            child(ChildStatus::Completed).encode(),
            descriptor().encode(),
            Evidence {
                kind: "push-success".to_string(),
                diagnostic: Some("ok".to_string()),
            }
            .encode(),
            publication.encode(),
        ];
        const EXPECTED: [&str; 22] = [
            concat!(
                r#"{"parent":"parent","parent_head":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","request":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","round":3,"tool":"tool"}"#,
                "\n"
            ),
            concat!(
                r#"{"id":"root","kind":"root","owner":{"parent":"parent","parent_head":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","request":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","round":3,"tool":"tool"}}"#,
                "\n"
            ),
            concat!(
                r#"{"source":"repo","source_tree":"dddddddddddddddddddddddddddddddddddddddd"}"#,
                "\n"
            ),
            concat!(
                r#"{"commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","initial":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","origin":{"source":"repo","source_tree":"dddddddddddddddddddddddddddddddddddddddd"}}"#,
                "\n"
            ),
            concat!(
                r#"{"arguments":{"path":".caos/transcript/arguments"},"id":"tool-1","name":"shell","type":"tool_use"}"#,
                "\n"
            ),
            concat!(
                r#"{"base":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","commit":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","workspace_name":"main"}"#,
                "\n"
            ),
            concat!(
                r#"{"base":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","implementation":"merge-v1","ours":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","output":"dddddddddddddddddddddddddddddddddddddddd","theirs":"cccccccccccccccccccccccccccccccccccccccc"}"#,
                "\n"
            ),
            concat!(
                r#"{"current":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","kind":"merged","merge":{"base":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","implementation":"merge-v1","ours":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","output":"dddddddddddddddddddddddddddddddddddddddd","theirs":"cccccccccccccccccccccccccccccccccccccccc"},"output":"dddddddddddddddddddddddddddddddddddddddd"}"#,
                "\n"
            ),
            concat!(
                r#"{"actor":"model","blocks":[{"arguments":{"path":".caos/transcript/arguments"},"id":"tool-1","name":"shell","type":"tool_use"}],"conversation":"conversation","message_id":"message-1","model":"model","proposal":{"base":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","commit":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","workspace_name":"main"},"request":"1111111111111111111111111111111111111111","role":"assistant","round":0,"workspace_resolution":{"current":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","kind":"direct","output":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
                "\n"
            ),
            concat!(r#"{"id":"tool-1","name":"shell"}"#, "\n"),
            concat!(
                r#"{"calls":[{"id":"tool-1","name":"shell"}],"configuration":"configuration-hash","escape_reason":"stop","id":"1111111111111111111111111111111111111111","interjections":["note"],"interrupted":true,"model":"model","request_head":"2222222222222222222222222222222222222222","request_workspaces":null,"result":".caos/requests/result","round":0,"status":"idle"}"#,
                "\n"
            ),
            concat!(
                r#"{"kind":"complete","observation":".caos/tools/observation","proposal":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#,
                "\n"
            ),
            concat!(r#"{"applied":["a.txt"],"conflicted":["b.txt"]}"#, "\n"),
            concat!(
                r#"{"declaration_message":"message-1","files":["a.txt"],"files_outcome":{"applied":["a.txt"],"conflicted":["b.txt"]},"id":"tool-1","input_workspace":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","name":"shell","request":"1111111111111111111111111111111111111111","result":{"kind":"complete","observation":".caos/tools/observation","proposal":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"round":0,"status":"complete","workspace_name":"main","workspace_resolution":{"current":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","kind":"direct","output":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
                "\n"
            ),
            concat!(
                r#"{"result":"2222222222222222222222222222222222222222","status":"complete","target_ref":"refs/heads/main","task":"1111111111111111111111111111111111111111"}"#,
                "\n"
            ),
            concat!(
                r#"{"configuration":"configuration-hash","files_seed":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","input_workspace":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","model":"model","prompt":".caos/tools/prompt","request":"1111111111111111111111111111111111111111","round":2,"tool":"tool-1","workspace_name":"main"}"#,
                "\n"
            ),
            concat!(
                r#"{"commit":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","initial":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
                "\n"
            ),
            concat!(
                r#"{"child_workspace":"main","parent_workspace":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","parent_workspace_name":"main","workspace_resolution":{"current":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","kind":"direct","output":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#,
                "\n"
            ),
            concat!(
                r#"{"applications":[{"child_workspace":"main","parent_workspace":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","parent_workspace_name":"main","workspace_resolution":{"current":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","kind":"direct","output":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}],"child_workspaces":{"main":{"commit":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","initial":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}},"id":"child-1","initial_head":"2222222222222222222222222222222222222222","initial_workspace":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","relay":"3333333333333333333333333333333333333333","request":"1111111111111111111111111111111111111111","spawn_intent":{"configuration":"configuration-hash","files_seed":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","input_workspace":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","model":"model","prompt":".caos/tools/prompt","request":"1111111111111111111111111111111111111111","round":2,"tool":"tool-1","workspace_name":"main"},"status":"completed","terminal_head":"4444444444444444444444444444444444444444"}"#,
                "\n"
            ),
            concat!(
                r#"{"commit_policy":"single","implementation":"project-v1","policy":"squash","source_base":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","source_head":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","target_base":"cccccccccccccccccccccccccccccccccccccccc"}"#,
                "\n"
            ),
            concat!(r#"{"diagnostic":"ok","kind":"push-success"}"#, "\n"),
            concat!(
                r#"{"descriptor":{"commit_policy":"single","implementation":"project-v1","policy":"squash","source_base":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","source_head":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","target_base":"cccccccccccccccccccccccccccccccccccccccc"},"evidence":{"diagnostic":"ok","kind":"push-success"},"expected_old":null,"id":"publication-1","key":"key","observed":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","planned_head":"dddddddddddddddddddddddddddddddddddddddd","ref":"refs/heads/main","repository":"repo","status":"complete","workspace_name":"main"}"#,
                "\n"
            ),
        ];
        for (actual, expected) in encoded.iter().zip(EXPECTED) {
            assert_eq!(actual, expected.as_bytes());
        }
    }

    macro_rules! round_trip {
        ($type:ty, $value:expr) => {{
            let value: $type = $value;
            assert_eq!(<$type>::parse(&value.encode()).unwrap(), value);
        }};
    }

    #[test]
    fn basic_records_round_trip() {
        let owner = Owner {
            parent: "parent".to_string(),
            parent_head: oid('a'),
            request: oid('b'),
            round: 3,
            tool: "tool".to_string(),
        };
        round_trip!(Owner, owner.clone());
        round_trip!(
            Identity,
            Identity {
                id: "root".to_string(),
                kind: IdentityKind::Root,
                owner: Some(owner),
            }
        );
        round_trip!(
            Identity,
            Identity {
                id: "fork".to_string(),
                kind: IdentityKind::Fork { source: oid('c') },
                owner: None,
            }
        );
        let origin = WorkspaceOrigin {
            source: "repo".to_string(),
            source_tree: oid('d'),
        };
        round_trip!(WorkspaceOrigin, origin.clone());
        round_trip!(
            WorkspaceRecord,
            WorkspaceRecord {
                commit: oid('a'),
                initial: oid('b'),
                origin: Some(origin),
            }
        );
        for block in [
            Block::Text {
                text: "hello".to_string(),
            },
            Block::Payload {
                path: ".caos/transcript/payload".to_string(),
            },
            Block::ToolUse {
                id: "tool-1".to_string(),
                name: "shell".to_string(),
                arguments: ".caos/transcript/arguments".to_string(),
            },
        ] {
            round_trip!(Block, block);
        }
        round_trip!(
            Proposal,
            Proposal {
                base: oid('a'),
                commit: oid('b'),
                workspace_name: "main".to_string(),
            }
        );
        round_trip!(SpawnIntent, spawn_intent());
        round_trip!(
            ChildWorkspace,
            ChildWorkspace {
                commit: oid('b'),
                initial: oid('a')
            }
        );
        round_trip!(
            Application,
            Application {
                parent_workspace_name: "main".to_string(),
                parent_workspace: Some(oid('a')),
                child_workspace: "main".to_string(),
                workspace_resolution: direct(),
            }
        );
        round_trip!(Descriptor, descriptor());
        round_trip!(
            Evidence,
            Evidence {
                kind: "push-success".to_string(),
                diagnostic: Some("ok".to_string())
            }
        );
        round_trip!(
            FilesOutcome,
            FilesOutcome {
                applied: vec!["a.txt".to_string()],
                conflicted: vec!["b.txt".to_string()]
            }
        );
    }

    #[test]
    fn merge_and_workspace_resolution_variants_round_trip() {
        let merged = MergeInfo {
            base: oid('a'),
            ours: oid('b'),
            theirs: oid('c'),
            implementation: "merge-v1".to_string(),
            output: Some(oid('d')),
            conflict_paths: None,
        };
        let conflict = MergeInfo {
            output: None,
            conflict_paths: Some(vec!["src/lib.rs".to_string()]),
            ..merged.clone()
        };
        round_trip!(MergeInfo, merged.clone());
        round_trip!(MergeInfo, conflict.clone());
        for resolution in [
            WorkspaceResolution::AlreadyApplied {
                current: oid('a'),
                candidate: None,
            },
            direct(),
            WorkspaceResolution::Merged {
                current: oid('b'),
                merge: merged,
                output: oid('d'),
            },
            WorkspaceResolution::Conflict {
                current: Some(oid('b')),
                candidate: oid('c'),
                merge: Some(conflict),
            },
        ] {
            round_trip!(WorkspaceResolution, resolution);
        }
    }

    #[test]
    fn transcript_roles_round_trip() {
        for role in [Role::User, Role::Assistant, Role::System] {
            round_trip!(
                TranscriptEntry,
                TranscriptEntry {
                    message_id: "message-1".to_string(),
                    conversation: "conversation".to_string(),
                    role,
                    actor: "actor".to_string(),
                    request: Some(oid('1')),
                    round: Some(0),
                    model: Some("model".to_string()),
                    blocks: vec![Block::Text {
                        text: "text".to_string()
                    }],
                    proposal: None,
                    workspace_resolution: None,
                }
            );
        }
        round_trip!(
            DeclaredCall,
            DeclaredCall {
                id: "tool-1".to_string(),
                name: "shell".to_string()
            }
        );
    }

    #[test]
    fn request_status_variants_round_trip() {
        let base = RequestRecord {
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
        };
        round_trip!(RequestRecord, base.clone());
        round_trip!(
            RequestRecord,
            RequestRecord {
                status: RequestStatus::Running,
                latest_message: Some("message-1".to_string()),
                ..base.clone()
            }
        );
        round_trip!(
            RequestRecord,
            RequestRecord {
                status: RequestStatus::Cancelling,
                latest_message: Some("message-1".to_string()),
                escape_reason: Some("stop".to_string()),
                ..base.clone()
            }
        );
        round_trip!(
            RequestRecord,
            RequestRecord {
                status: RequestStatus::Idle,
                escape_reason: Some("stop".to_string()),
                outcome: Some(RequestOutcome::Idle {
                    result: Some(".caos/requests/result".to_string()),
                    interrupted: true,
                }),
                ..base.clone()
            }
        );
        round_trip!(
            RequestRecord,
            RequestRecord {
                status: RequestStatus::Failed,
                outcome: Some(RequestOutcome::Failed {
                    error: ".caos/requests/error".to_string()
                }),
                ..base
            }
        );
    }

    #[test]
    fn configurations_are_opaque_nonempty_strings() {
        let mut intent = spawn_intent();
        intent.configuration = "opaque value that is not a tree path".to_string();
        round_trip!(SpawnIntent, intent.clone());
        intent.configuration.clear();
        assert!(SpawnIntent::from_value(&intent.to_value()).is_err());

        let mut request = RequestRecord {
            id: oid('1'),
            request_head: oid('2'),
            request_workspaces: None,
            model: "model".to_string(),
            configuration: "opaque value that is not a tree path".to_string(),
            round: 0,
            calls: Vec::new(),
            interjections: Vec::new(),
            status: RequestStatus::Queued,
            latest_message: None,
            escape_reason: None,
            outcome: None,
        };
        round_trip!(RequestRecord, request.clone());
        request.configuration.clear();
        assert!(RequestRecord::from_value(&request.to_value()).is_err());
    }

    #[test]
    fn failed_requests_forbid_escape_reason() {
        let request = RequestRecord {
            id: oid('1'),
            request_head: oid('2'),
            request_workspaces: None,
            model: "model".to_string(),
            configuration: "configuration-hash".to_string(),
            round: 0,
            calls: Vec::new(),
            interjections: Vec::new(),
            status: RequestStatus::Failed,
            latest_message: None,
            escape_reason: Some("stop".to_string()),
            outcome: Some(RequestOutcome::Failed {
                error: ".caos/requests/error".to_string(),
            }),
        };
        assert!(RequestRecord::from_value(&request.to_value()).is_err());
    }

    #[test]
    fn every_record_rejects_unknown_keys() {
        macro_rules! rejects_unknown {
            ($type:ty, $value:expr) => {{
                let mut value = $value.to_value();
                value
                    .as_object_mut()
                    .unwrap()
                    .insert("unknown".to_string(), Value::Null);
                assert!(
                    <$type>::from_value(&value).is_err(),
                    "{} accepted an unknown key",
                    stringify!($type)
                );
            }};
        }

        let owner = Owner {
            parent: "parent".to_string(),
            parent_head: oid('a'),
            request: oid('b'),
            round: 0,
            tool: "tool".to_string(),
        };
        let origin = WorkspaceOrigin {
            source: "repo".to_string(),
            source_tree: oid('a'),
        };
        let merge = MergeInfo {
            base: oid('a'),
            ours: oid('b'),
            theirs: oid('c'),
            implementation: "merge-v1".to_string(),
            output: Some(oid('d')),
            conflict_paths: None,
        };
        let request = RequestRecord {
            id: oid('1'),
            request_head: oid('2'),
            request_workspaces: None,
            model: "model".to_string(),
            configuration: "configuration".to_string(),
            round: 0,
            calls: Vec::new(),
            interjections: Vec::new(),
            status: RequestStatus::Queued,
            latest_message: None,
            escape_reason: None,
            outcome: None,
        };
        let tool = ToolRecord {
            request: oid('1'),
            round: 0,
            id: "tool-1".to_string(),
            name: "shell".to_string(),
            declaration_message: "message-1".to_string(),
            workspace_name: None,
            input_workspace: None,
            status: ToolStatus::Started,
            task: Some(oid('2')),
            result: None,
            workspace_resolution: None,
            files: Vec::new(),
            files_outcome: None,
        };
        let application = Application {
            parent_workspace_name: "main".to_string(),
            parent_workspace: None,
            child_workspace: "main".to_string(),
            workspace_resolution: direct(),
        };
        let publication = PublicationRecord {
            id: "publication-1".to_string(),
            key: "key".to_string(),
            descriptor: descriptor(),
            planned_head: oid('d'),
            repository: "repo".to_string(),
            refname: "refs/heads/main".to_string(),
            expected_old: None,
            workspace_name: "main".to_string(),
            status: PublicationStatus::Pending,
            evidence: None,
            observed: None,
        };

        rejects_unknown!(Owner, owner.clone());
        rejects_unknown!(
            Identity,
            Identity {
                id: "root".to_string(),
                kind: IdentityKind::Root,
                owner: Some(owner),
            }
        );
        rejects_unknown!(WorkspaceOrigin, origin.clone());
        rejects_unknown!(
            WorkspaceRecord,
            WorkspaceRecord {
                commit: oid('a'),
                initial: oid('b'),
                origin: Some(origin),
            }
        );
        rejects_unknown!(
            Block,
            Block::Text {
                text: "text".to_string(),
            }
        );
        rejects_unknown!(
            Proposal,
            Proposal {
                base: oid('a'),
                commit: oid('b'),
                workspace_name: "main".to_string(),
            }
        );
        rejects_unknown!(MergeInfo, merge.clone());
        rejects_unknown!(
            WorkspaceResolution,
            WorkspaceResolution::Merged {
                current: oid('b'),
                merge,
                output: oid('d'),
            }
        );
        rejects_unknown!(
            TranscriptEntry,
            TranscriptEntry {
                message_id: "message-1".to_string(),
                conversation: "conversation".to_string(),
                role: Role::User,
                actor: "user".to_string(),
                request: None,
                round: None,
                model: None,
                blocks: Vec::new(),
                proposal: None,
                workspace_resolution: None,
            }
        );
        rejects_unknown!(
            DeclaredCall,
            DeclaredCall {
                id: "tool-1".to_string(),
                name: "shell".to_string(),
            }
        );
        rejects_unknown!(RequestRecord, request);
        rejects_unknown!(
            ToolResult,
            ToolResult::Cancelled {
                reason: "stop".to_string(),
            }
        );
        rejects_unknown!(
            FilesOutcome,
            FilesOutcome {
                applied: Vec::new(),
                conflicted: Vec::new(),
            }
        );
        rejects_unknown!(ToolRecord, tool);
        rejects_unknown!(
            AsyncRecord,
            AsyncRecord {
                task: oid('1'),
                status: AsyncStatus::Pending,
                target_ref: None,
                result: None,
                reason: None,
            }
        );
        rejects_unknown!(SpawnIntent, spawn_intent());
        rejects_unknown!(
            ChildWorkspace,
            ChildWorkspace {
                commit: oid('b'),
                initial: oid('a'),
            }
        );
        rejects_unknown!(Application, application);
        rejects_unknown!(ChildRecord, child(ChildStatus::Running));
        rejects_unknown!(Descriptor, descriptor());
        rejects_unknown!(
            Evidence,
            Evidence {
                kind: "push-success".to_string(),
                diagnostic: None,
            }
        );
        rejects_unknown!(PublicationRecord, publication);
    }

    #[test]
    fn optional_key_nullability_is_exact() {
        macro_rules! rejects_null {
            ($type:ty, $value:expr, $key:literal) => {{
                let mut value = $value.to_value();
                value
                    .as_object_mut()
                    .unwrap()
                    .insert($key.to_string(), Value::Null);
                assert!(
                    <$type>::from_value(&value).is_err(),
                    "{} accepted null for {}",
                    stringify!($type),
                    $key
                );
            }};
        }
        macro_rules! rejects_missing {
            ($type:ty, $value:expr, $key:literal) => {{
                let mut value = $value.to_value();
                value.as_object_mut().unwrap().remove($key);
                assert!(
                    <$type>::from_value(&value).is_err(),
                    "{} accepted missing {}",
                    stringify!($type),
                    $key
                );
            }};
        }

        let transcript = TranscriptEntry {
            message_id: "message-1".to_string(),
            conversation: "conversation".to_string(),
            role: Role::User,
            actor: "user".to_string(),
            request: None,
            round: None,
            model: None,
            blocks: Vec::new(),
            proposal: None,
            workspace_resolution: None,
        };
        rejects_null!(TranscriptEntry, transcript.clone(), "request");
        rejects_null!(TranscriptEntry, transcript.clone(), "round");
        rejects_null!(TranscriptEntry, transcript, "model");

        let queued = RequestRecord {
            id: oid('1'),
            request_head: oid('2'),
            request_workspaces: None,
            model: "model".to_string(),
            configuration: "configuration".to_string(),
            round: 0,
            calls: Vec::new(),
            interjections: Vec::new(),
            status: RequestStatus::Queued,
            latest_message: None,
            escape_reason: None,
            outcome: None,
        };
        rejects_null!(
            RequestRecord,
            RequestRecord {
                status: RequestStatus::Running,
                latest_message: Some("message-1".to_string()),
                ..queued.clone()
            },
            "latest_message"
        );
        rejects_null!(
            RequestRecord,
            RequestRecord {
                status: RequestStatus::Idle,
                outcome: Some(RequestOutcome::Idle {
                    result: None,
                    interrupted: false,
                }),
                ..queued.clone()
            },
            "escape_reason"
        );
        for key in ["target_ref", "result", "error", "reason"] {
            let mut value = AsyncRecord {
                task: oid('1'),
                status: AsyncStatus::Pending,
                target_ref: None,
                result: None,
                reason: None,
            }
            .to_value();
            value
                .as_object_mut()
                .unwrap()
                .insert(key.to_string(), Value::Null);
            assert!(AsyncRecord::from_value(&value).is_err(), "accepted {key}");
        }
        rejects_null!(
            ToolResult,
            ToolResult::Complete {
                observation: ".caos/tools/observation".to_string(),
                proposal: None,
            },
            "proposal"
        );
        rejects_null!(
            MergeInfo,
            MergeInfo {
                base: oid('a'),
                ours: oid('b'),
                theirs: oid('c'),
                implementation: "merge-v1".to_string(),
                output: None,
                conflict_paths: Some(vec!["file".to_string()]),
            },
            "output"
        );
        rejects_null!(
            WorkspaceResolution,
            WorkspaceResolution::AlreadyApplied {
                current: oid('a'),
                candidate: None,
            },
            "candidate"
        );
        rejects_null!(
            Evidence,
            Evidence {
                kind: "push-success".to_string(),
                diagnostic: None,
            },
            "diagnostic"
        );
        rejects_null!(ChildRecord, child(ChildStatus::Running), "terminal_head");

        rejects_missing!(
            WorkspaceResolution,
            WorkspaceResolution::Conflict {
                current: None,
                candidate: oid('b'),
                merge: None,
            },
            "current"
        );
        rejects_missing!(RequestRecord, queued.clone(), "request_workspaces");
        rejects_missing!(
            RequestRecord,
            RequestRecord {
                status: RequestStatus::Idle,
                outcome: Some(RequestOutcome::Idle {
                    result: None,
                    interrupted: false,
                }),
                ..queued
            },
            "result"
        );
        let started_tool = ToolRecord {
            request: oid('1'),
            round: 0,
            id: "tool-1".to_string(),
            name: "shell".to_string(),
            declaration_message: "message-1".to_string(),
            workspace_name: None,
            input_workspace: None,
            status: ToolStatus::Started,
            task: Some(oid('2')),
            result: None,
            workspace_resolution: None,
            files: Vec::new(),
            files_outcome: None,
        };
        rejects_missing!(ToolRecord, started_tool.clone(), "workspace_name");
        rejects_missing!(ToolRecord, started_tool, "input_workspace");
        let intent = SpawnIntent {
            workspace_name: None,
            input_workspace: None,
            files_seed: None,
            ..spawn_intent()
        };
        rejects_missing!(SpawnIntent, intent.clone(), "workspace_name");
        rejects_missing!(SpawnIntent, intent.clone(), "input_workspace");
        rejects_missing!(SpawnIntent, intent, "files_seed");
        rejects_missing!(
            Application,
            Application {
                parent_workspace_name: "main".to_string(),
                parent_workspace: None,
                child_workspace: "main".to_string(),
                workspace_resolution: direct(),
            },
            "parent_workspace"
        );
        rejects_missing!(
            ChildRecord,
            ChildRecord {
                initial_workspace: None,
                ..child(ChildStatus::Running)
            },
            "initial_workspace"
        );
        let terminal_child = child(ChildStatus::Completed);
        rejects_missing!(ChildRecord, terminal_child.clone(), "terminal_head");
        rejects_missing!(ChildRecord, terminal_child, "child_workspaces");
        let pending_publication = PublicationRecord {
            id: "publication-1".to_string(),
            key: "key".to_string(),
            descriptor: descriptor(),
            planned_head: oid('d'),
            repository: "repo".to_string(),
            refname: "refs/heads/main".to_string(),
            expected_old: None,
            workspace_name: "main".to_string(),
            status: PublicationStatus::Pending,
            evidence: None,
            observed: None,
        };
        rejects_missing!(
            PublicationRecord,
            pending_publication.clone(),
            "expected_old"
        );
        rejects_missing!(
            PublicationRecord,
            PublicationRecord {
                status: PublicationStatus::Complete,
                evidence: Some(Evidence {
                    kind: "push-success".to_string(),
                    diagnostic: None,
                }),
                ..pending_publication
            },
            "observed"
        );
        rejects_missing!(
            ToolRecord,
            ToolRecord {
                status: ToolStatus::Complete,
                task: None,
                result: Some(ToolResult::Complete {
                    observation: ".caos/tools/observation".to_string(),
                    proposal: None,
                }),
                ..ToolRecord {
                    request: oid('1'),
                    round: 0,
                    id: "tool-1".to_string(),
                    name: "shell".to_string(),
                    declaration_message: "message-1".to_string(),
                    workspace_name: None,
                    input_workspace: None,
                    status: ToolStatus::Started,
                    task: Some(oid('2')),
                    result: None,
                    workspace_resolution: None,
                    files: Vec::new(),
                    files_outcome: None,
                }
            },
            "files_outcome"
        );
    }

    #[test]
    fn tool_status_variants_round_trip() {
        let base = ToolRecord {
            request: oid('1'),
            round: 0,
            id: "tool-1".to_string(),
            name: "shell".to_string(),
            declaration_message: "message-1".to_string(),
            workspace_name: None,
            input_workspace: None,
            status: ToolStatus::Started,
            task: Some(oid('2')),
            result: None,
            workspace_resolution: None,
            files: Vec::new(),
            files_outcome: None,
        };
        round_trip!(ToolRecord, base.clone());
        for (status, result) in [
            (
                ToolStatus::Complete,
                ToolResult::Complete {
                    observation: ".caos/tools/observation".to_string(),
                    proposal: None,
                },
            ),
            (
                ToolStatus::Failed,
                ToolResult::Failed {
                    error: ".caos/tools/error".to_string(),
                },
            ),
            (
                ToolStatus::Cancelled,
                ToolResult::Cancelled {
                    reason: "stop".to_string(),
                },
            ),
        ] {
            round_trip!(
                ToolRecord,
                ToolRecord {
                    status,
                    result: Some(result),
                    ..base.clone()
                }
            );
        }
        round_trip!(
            ToolRecord,
            ToolRecord {
                workspace_name: Some("main".to_string()),
                input_workspace: Some(oid('a')),
                status: ToolStatus::Conflict,
                result: Some(ToolResult::Complete {
                    observation: ".caos/tools/observation".to_string(),
                    proposal: Some(oid('b')),
                }),
                workspace_resolution: Some(WorkspaceResolution::Conflict {
                    current: Some(oid('a')),
                    candidate: oid('b'),
                    merge: None,
                }),
                ..base
            }
        );
    }

    #[test]
    fn async_child_and_publication_status_variants_round_trip() {
        for record in [
            AsyncRecord {
                task: oid('1'),
                status: AsyncStatus::Pending,
                target_ref: None,
                result: None,
                reason: None,
            },
            AsyncRecord {
                task: oid('1'),
                status: AsyncStatus::Complete,
                target_ref: Some("refs/heads/main".to_string()),
                result: Some(oid('2')),
                reason: None,
            },
            AsyncRecord {
                task: oid('1'),
                status: AsyncStatus::Failed,
                target_ref: None,
                result: Some(oid('2')),
                reason: None,
            },
            AsyncRecord {
                task: oid('1'),
                status: AsyncStatus::Cancelled,
                target_ref: None,
                result: None,
                reason: Some("stop".to_string()),
            },
        ] {
            round_trip!(AsyncRecord, record);
        }
        for status in [
            ChildStatus::Running,
            ChildStatus::Completed,
            ChildStatus::Failed,
            ChildStatus::Cancelled,
        ] {
            round_trip!(ChildRecord, child(status));
        }
        for status in [
            PublicationStatus::Pending,
            PublicationStatus::Complete,
            PublicationStatus::Conflict,
            PublicationStatus::Uncertain,
        ] {
            round_trip!(
                PublicationRecord,
                PublicationRecord {
                    id: "publication-1".to_string(),
                    key: "key".to_string(),
                    descriptor: descriptor(),
                    planned_head: oid('d'),
                    repository: "repo".to_string(),
                    refname: "refs/heads/main".to_string(),
                    expected_old: None,
                    workspace_name: "main".to_string(),
                    status,
                    evidence: (status != PublicationStatus::Pending).then(|| Evidence {
                        kind: "push-success".to_string(),
                        diagnostic: None,
                    }),
                    observed: None,
                }
            );
        }
    }

    #[test]
    fn scalar_encodings_and_strict_keys_are_enforced() {
        assert_eq!(
            parse_title(&encode_title("Title").unwrap()).unwrap(),
            "Title"
        );
        assert!(encode_title("").is_err());
        assert!(encode_title("bad\n").is_err());
        let hash = oid('a');
        assert_eq!(parse_active_request(&hash.encode_line()), Ok(hash));
        assert!(parse_active_request(b"aaaa\n").is_err());
        assert!(Owner::from_value(&obj(&[("unknown", Value::Null)])).is_err());
        assert!(Evidence::from_value(&obj(&[("kind", value_str("push-success"))])).is_ok());
    }
}
