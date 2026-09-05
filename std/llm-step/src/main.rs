//! The agent turn driver for the v3 conversation protocol.

mod async_work;
mod githist;
mod progress;
mod subagents;
mod timing;
mod tools;
mod workspaces;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use conversation_protocol::v3::apply::{apply, inherited_signature, mint, Transition};
use conversation_protocol::v3::canonical::canonical_bytes;
use conversation_protocol::v3::ids;
use conversation_protocol::v3::paths;
use conversation_protocol::v3::view::Conversation;
use conversation_protocol::v3::{
    reconcile, validate_spine, Application, AsyncRecord, AsyncStatus, Block, ChildRecord,
    ChildStatus, CodeOps, DeclaredCall, FilesOutcome, Identity, IdentityKind, Kind, Mode,
    ObjectStore, Oid, Owner, RequestOutcome, RequestRecord, RequestStatus, Role, SpawnIntent,
    ToolRecord, ToolResult, ToolStatus, TranscriptEntry, WorkspaceResolution,
};
use llm_client::{post_messages, DEFAULT_BASE_URL};
use serde_json::{json, Value};
use worker_common::{
    arg, caos, caos_curry, caos_recurry, cas_hash, eval_then_catching, link, own_args_tree, path,
    prepare_request, read_arg, read_arg_opt, run_request_then_catching, run_worker, scratch,
    secret, Arg,
};

const MAX_TOKENS: u64 = 64000;
const MAX_CONTINUATIONS: u32 = 8;
const MAX_SPINE_WALK: usize = 4096;
static VALID_ADMISSIONS: OnceLock<Mutex<HashSet<(Oid, Oid)>>> = OnceLock::new();
const STD_TOOLS: [(&str, &str); 3] = [
    ("caos-build", "caos-build-image"),
    ("caos-test", "caos-test-image"),
    ("caos-test-result", "caos-test-result-image"),
];

fn main() -> std::process::ExitCode {
    timing::start();
    let exit = run_worker("llm-step", run);
    timing::phase("exit");
    exit
}

struct Config {
    api_key: String,
    system: String,
    bash_image: String,
    grep_image: Option<String>,
    tools_image: Option<String>,
    merge_image: Option<String>,
    std_tool_images: BTreeMap<&'static str, Option<String>>,
    run_and_update_ref_image: Option<String>,
    merge_refs: Option<String>,
    model: String,
    base_url: String,
    conversation: String,
    focus_workspace: Option<String>,
}

fn image_arg(name: &str) -> Result<Option<String>, String> {
    let value = arg(name);
    if Path::new(&value).exists() {
        cas_hash(&value).map(Some)
    } else {
        Ok(None)
    }
}

impl Config {
    fn read() -> Result<Self, String> {
        let run_and_update_ref_image = if read_arg_opt("subagent")?.is_some() {
            None
        } else {
            image_arg("run-and-update-ref-image")?
        };
        Ok(Self {
            api_key: secret("anthropic-api-key")?,
            system: read_arg("system")?,
            focus_workspace: read_arg_opt("focus-workspace")?,
            bash_image: image_arg("bash-image")?.ok_or("--bash-image is required")?,
            grep_image: image_arg("grep-image")?,
            tools_image: image_arg("tools-image")?,
            merge_image: image_arg("merge-image")?,
            std_tool_images: STD_TOOLS
                .iter()
                .map(|&(name, argument)| Ok((name, image_arg(argument)?)))
                .collect::<Result<_, String>>()?,
            run_and_update_ref_image,
            merge_refs: read_arg_opt("merge-refs")?,
            model: read_arg("model")?,
            base_url: read_arg_opt("base-url")?.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            conversation: read_arg_opt("conversation")?
                .ok_or_else(|| "llm-step requires --conversation".to_string())?,
        })
    }
}

fn run() -> Result<(), String> {
    let cfg = Config::read()?;
    let run_text = read_arg_opt("run")?.unwrap_or(own_args_tree()?);
    let request = Oid::parse(&run_text, "conversation request")?;
    let request_head = Oid::parse(&cas_hash(&arg("head"))?, "request head")?;
    let mut state = progress::State::open(&cfg.conversation)?;
    timing::phase("state.open");
    let outcome = if Path::new(&arg("result")).exists() || Path::new(&arg("error")).exists() {
        callback(&cfg, &mut state, &request, &request_head)
    } else {
        start(&cfg, &mut state, &request, &request_head)
    };
    if let Err(error) = &outcome {
        if let Err(record_error) = record_failure(&cfg, &mut state, &request, error) {
            eprintln!("llm-step: additionally failed to record failure: {record_error}");
        }
    }
    outcome
}

fn start(
    cfg: &Config,
    state: &mut progress::State,
    request: &Oid,
    request_head: &Oid,
) -> Result<(), String> {
    loop {
        validate_admission(state, request, request_head)?;
        let record = require_request(&state.conversation()?, request)?;
        match record.status {
            RequestStatus::Queued => {
                let latest_message = newest_user_message(&state.conversation()?)?;
                let expected = state.head().clone();
                match state.try_append_at(
                    &expected,
                    Transition::RequestClaim {
                        request: request.clone(),
                        latest_message,
                    },
                )? {
                    progress::TryAppend::Appended(_) => break,
                    progress::TryAppend::HeadChanged(_) => continue,
                }
            }
            RequestStatus::Running => break,
            RequestStatus::Cancelling => return drain(state, request),
            RequestStatus::Idle | RequestStatus::Failed => {
                return finish_from_terminal(state, request)
            }
        }
    }
    reconcile_background_tasks(state)?;
    announce_background_tasks(&cfg.conversation, state)?;
    resume(cfg, state, request, request_head)
}

fn validate_admission(
    state: &mut progress::State,
    request: &Oid,
    request_head: &Oid,
) -> Result<(), String> {
    let key = (request.clone(), state.head().clone());
    {
        let validated = VALID_ADMISSIONS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| "admission validation cache is poisoned".to_string())?;
        if validated.contains(&key) {
            return Ok(());
        }
    }
    let view = state.conversation()?;
    let identity = view.identity()?;
    if identity.id != state_conversation_id(state)? {
        return Err(format!(
            "conversation identity {:?} does not match its ref",
            identity.id
        ));
    }
    let record = require_request(&view, request)?;
    if record.request_head != *request_head {
        return Err(format!(
            "request {request} records head {}, not {request_head}",
            record.request_head
        ));
    }
    if !state.store().is_ancestor(request_head, state.head())? {
        return Err(format!(
            "request head {request_head} is not on the conversation spine"
        ));
    }
    let at_request = state.conversation_at(request_head)?;
    if at_request.workspaces_tree()? != record.request_workspaces {
        return Err("request workspaces diverged".to_string());
    }
    VALID_ADMISSIONS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "admission validation cache is poisoned".to_string())?
        .insert(key);
    Ok(())
}

fn state_conversation_id(state: &progress::State) -> Result<String, String> {
    conversation_protocol::v3::refs::parse_head_ref(state.refname())
}

fn require_request(view: &Conversation<'_>, request: &Oid) -> Result<RequestRecord, String> {
    view.request(request)?
        .ok_or_else(|| format!("conversation has no request record for {request}"))
}

fn last_matching<T>(
    view: &Conversation<'_>,
    mut select: impl FnMut(u64, TranscriptEntry) -> Option<T>,
) -> Result<Option<T>, String> {
    for ordinal in (0..view.transcript_len()?).rev() {
        let (_, entry) = view
            .transcript_entry(ordinal)?
            .ok_or_else(|| format!("missing transcript ordinal {ordinal}"))?;
        if let Some(result) = select(ordinal, entry) {
            return Ok(Some(result));
        }
    }
    Ok(None)
}

fn newest_user_message(view: &Conversation<'_>) -> Result<String, String> {
    last_matching(view, |_, entry| {
        (entry.role == Role::User).then_some(entry.message_id)
    })?
    .ok_or_else(|| "request has no user message in the transcript".to_string())
}

fn callback(
    cfg: &Config,
    state: &mut progress::State,
    request: &Oid,
    request_head: &Oid,
) -> Result<(), String> {
    validate_admission(state, request, request_head)?;
    let status = require_request(&state.conversation()?, request)?.status;
    if matches!(status, RequestStatus::Idle | RequestStatus::Failed) {
        return finish_from_terminal(state, request);
    }
    let round = read_arg("round")?
        .parse::<u64>()
        .map_err(|error| format!("invalid continuation round: {error}"))?;
    let id = read_arg("current-id")?;
    let tool = read_arg_opt("current-tool")?.unwrap_or_else(|| "bash".to_string());
    timing::phase(&format!("tool wait {tool}"));

    if read_arg_opt("tool-eval")?.is_some() {
        if Path::new(&arg("error")).exists() {
            let error = read_arg("error")?;
            let block = failed_run_block(&id, &tool, &error);
            let target = pending_call_target(state, request, round, &id)?;
            let declaration = declaration_message(&state.conversation()?, request, round)?;
            let call = Call {
                id,
                name: tool,
                input: Value::Null,
            };
            CallSite::at(request, round, &call, &declaration).failed(state, &block, target)?;
            return resume(cfg, state, request, request_head);
        }
        return launch_evaluated_tool(cfg, state, request, request_head, round, &id, &tool);
    }

    let record = state
        .conversation()?
        .tool(request, round, &id)?
        .ok_or_else(|| format!("callback for {request}/{round}/{id} has no tool.start"))?;
    if record.is_terminal() {
        return resume(cfg, state, request, request_head);
    }
    if record.name != tool {
        return Err(format!(
            "callback tool {tool:?} does not match recorded tool {:?}",
            record.name
        ));
    }
    if Path::new(&arg("error")).exists() {
        let error = read_arg("error")?;
        let block = failed_run_block(&id, &tool, &error);
        complete_started_failed(state, &record, &block)?;
        return resume(cfg, state, request, request_head);
    }

    let (block, proposal) = callback_result(state, &record)?;
    complete_compute(state, &record, block, proposal)?;
    resume(cfg, state, request, request_head)
}

fn pending_call_target(
    state: &progress::State,
    request: &Oid,
    round: u64,
    id: &str,
) -> Result<Option<(String, Oid)>, String> {
    let view = state.conversation()?;
    let record = require_request(&view, request)?;
    let round_state = round_state(&view, &record)?;
    if round_state.declaring_round != round {
        return Ok(None);
    }
    let Some(call) = round_state.pending.iter().find(|call| call.id == id) else {
        return Ok(None);
    };
    Ok(match resolve_target(&view, call)? {
        Target::Workspace { name, commit } => Some((name, commit)),
        Target::Files => None,
    })
}

fn resume(
    cfg: &Config,
    state: &mut progress::State,
    request: &Oid,
    request_head: &Oid,
) -> Result<(), String> {
    loop {
        if !state.take_fresh_after_append() {
            state.reload()?;
        }
        validate_admission(state, request, request_head)?;
        let record = require_request(&state.conversation()?, request)?;
        match record.status {
            RequestStatus::Cancelling => return drain(state, request),
            RequestStatus::Idle | RequestStatus::Failed => {
                return finish_from_terminal(state, request)
            }
            RequestStatus::Queued => return start(cfg, state, request, request_head),
            RequestStatus::Running => {}
        }

        let round = round_state(&state.conversation()?, &record)?;
        if let Some(call) = round.pending.first() {
            if drive_call(cfg, state, request, &round, call)? {
                continue;
            }
            return Ok(());
        }

        reconcile_background_tasks(state)?;
        announce_background_tasks(&cfg.conversation, state)?;
        state.reload()?;
        let current = require_request(&state.conversation()?, request)?;
        if current.status != RequestStatus::Running {
            continue;
        }
        let round = round_state(&state.conversation()?, &current)?;
        if !round.pending.is_empty() {
            continue;
        }
        let messages = context_messages(&state.conversation()?)?;
        let workspaces = workspace_paths(state)?;
        let previous = state.head().clone();
        return llm_round(
            cfg,
            state,
            request,
            request_head,
            messages,
            &workspaces,
            &previous,
            current.round,
        );
    }
}

#[derive(Clone)]
struct Call {
    id: String,
    name: String,
    input: Value,
}

impl Call {
    fn value(&self) -> Value {
        json!({"type":"tool_use", "id":self.id, "name":self.name, "input":self.input})
    }
}

struct RoundState {
    declaring_round: u64,
    declaration_message: String,
    pending: Vec<Call>,
}

struct CallSite<'a> {
    request: &'a Oid,
    round: u64,
    call: &'a Call,
    declaration: &'a str,
}

impl<'a> CallSite<'a> {
    fn new(request: &'a Oid, round: &'a RoundState, call: &'a Call) -> Self {
        Self::at(
            request,
            round.declaring_round,
            call,
            &round.declaration_message,
        )
    }

    fn at(request: &'a Oid, round: u64, call: &'a Call, declaration: &'a str) -> Self {
        Self {
            request,
            round,
            call,
            declaration,
        }
    }

    fn stub(&self, target: Option<(String, Oid)>) -> ToolRecord {
        let (workspace_name, input_workspace) = match target {
            Some((name, commit)) => (Some(name), Some(commit)),
            None => (None, None),
        };
        ToolRecord {
            request: self.request.clone(),
            round: self.round,
            id: self.call.id.clone(),
            name: self.call.name.clone(),
            declaration_message: self.declaration.to_string(),
            workspace_name,
            input_workspace,
            status: ToolStatus::Complete,
            task: None,
            result: None,
            workspace_resolution: None,
            files: Vec::new(),
            files_outcome: None,
        }
    }

    fn finish(
        &self,
        state: &mut progress::State,
        block: &Value,
        target: Option<(String, Oid)>,
        resolution: Option<WorkspaceResolution>,
        result: impl FnOnce(&ToolRecord) -> ToolResult,
    ) -> Result<(), String> {
        let stub = self.stub(target);
        let record = completed_record(&stub, result(&stub), resolution);
        let expected = state.head().clone();
        let _ = state.try_append_at(
            &expected,
            tool_complete_transition(record, block, Vec::new())?,
        )?;
        Ok(())
    }

    fn complete(
        &self,
        state: &mut progress::State,
        block: Value,
        target: Option<(String, Oid)>,
        proposal: Option<Oid>,
        resolution: Option<WorkspaceResolution>,
    ) -> Result<(), String> {
        self.finish(state, &block, target, resolution, |stub| {
            ToolResult::Complete {
                observation: observation_path(stub),
                proposal,
            }
        })
    }

    fn fail(&self, state: &mut progress::State, text: &str) -> Result<(), String> {
        self.complete(state, error_block(&self.call.id, text), None, None, None)
    }

    fn failed(
        &self,
        state: &mut progress::State,
        block: &Value,
        target: Option<(String, Oid)>,
    ) -> Result<(), String> {
        self.finish(state, block, target, None, |stub| ToolResult::Failed {
            error: observation_path(stub),
        })
    }
}

fn close_pending_call(
    state: &mut progress::State,
    request: &Oid,
    round: &RoundState,
    call: &Call,
    block: &Value,
    result: impl FnOnce(&ToolRecord) -> ToolResult,
) -> Result<(), String> {
    let existing = state
        .conversation()?
        .tool(request, round.declaring_round, &call.id)?;
    let target = existing
        .as_ref()
        .and_then(|record| {
            record
                .workspace_name
                .clone()
                .zip(record.input_workspace.clone())
        })
        .or_else(
            || match resolve_target(&state.conversation().ok()?, call).ok()? {
                Target::Workspace { name, commit } => Some((name, commit)),
                Target::Files => None,
            },
        );
    let stub = existing.unwrap_or_else(|| CallSite::new(request, round, call).stub(target));
    let record = completed_record(&stub, result(&stub), None);
    let expected = state.head().clone();
    let _ = state.try_append_at(
        &expected,
        tool_complete_transition(record, block, Vec::new())?,
    )?;
    Ok(())
}

fn round_state(view: &Conversation<'_>, record: &RequestRecord) -> Result<RoundState, String> {
    if record.round == 0 {
        if !record.calls.is_empty() {
            return Err("round-zero request carries declared calls".to_string());
        }
        return Ok(RoundState {
            declaring_round: 0,
            declaration_message: String::new(),
            pending: Vec::new(),
        });
    }
    let declaring_round = record.round - 1;
    let mut declaration = None;
    for ordinal in (0..view.transcript_len()?).rev() {
        let (_, entry) = view
            .transcript_entry(ordinal)?
            .ok_or_else(|| format!("missing transcript ordinal {ordinal}"))?;
        if entry.role == Role::Assistant
            && entry.request.as_ref() == Some(&record.id)
            && entry.round == Some(declaring_round)
        {
            declaration = Some((ordinal, entry));
            break;
        }
    }
    let (ordinal, entry) = declaration.ok_or_else(|| {
        format!(
            "request {} round {declaring_round} has no assistant declaration",
            record.id
        )
    })?;
    let mut calls = Vec::new();
    for block in &entry.blocks {
        if let Block::ToolUse {
            id,
            name,
            arguments,
        } = block
        {
            let input = serde_json::from_slice(&view.payload(arguments)?)
                .map_err(|error| format!("parsing tool arguments {arguments}: {error}"))?;
            calls.push(Call {
                id: id.clone(),
                name: name.clone(),
                input,
            });
        }
    }
    let projection: Vec<DeclaredCall> = calls
        .iter()
        .map(|call| DeclaredCall {
            id: call.id.clone(),
            name: call.name.clone(),
        })
        .collect();
    if projection != record.calls {
        return Err(format!(
            "request {} calls do not match transcript ordinal {ordinal}",
            record.id
        ));
    }
    let mut pending = Vec::new();
    for call in calls {
        if !view
            .tool(&record.id, declaring_round, &call.id)?
            .is_some_and(|tool| tool.is_terminal())
        {
            pending.push(call);
        }
    }
    Ok(RoundState {
        declaring_round,
        declaration_message: entry.message_id,
        pending,
    })
}

fn context_messages(view: &Conversation<'_>) -> Result<Vec<Value>, String> {
    let mut messages = Vec::new();
    for ordinal in 0..view.transcript_len()? {
        let (_, entry) = view
            .transcript_entry(ordinal)?
            .ok_or_else(|| format!("missing transcript ordinal {ordinal}"))?;
        match entry.role {
            Role::User | Role::System => {
                let text = entry
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        Block::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                messages.push(user_text(&text));
            }
            Role::Assistant => {
                let response_path = format!(
                    "{}/response.json",
                    paths::transcript_payload_dir(ordinal, &entry.message_id)
                );
                let response: Value = serde_json::from_slice(&view.payload(&response_path)?)
                    .map_err(|error| format!("parsing {response_path}: {error}"))?;
                let blocks = response
                    .as_array()
                    .cloned()
                    .ok_or_else(|| format!("{response_path} is not a JSON array"))?;
                messages.push(message("assistant", Value::Array(blocks)));
                let calls: Vec<(&str, &str)> = entry
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        Block::ToolUse { id, .. } => Some((
                            entry.request.as_ref().map(Oid::as_str).unwrap_or_default(),
                            id.as_str(),
                        )),
                        _ => None,
                    })
                    .collect();
                if calls.is_empty() {
                    continue;
                }
                let request = entry.request.as_ref().ok_or_else(|| {
                    format!("assistant entry {} has no request", entry.message_id)
                })?;
                let round = entry
                    .round
                    .ok_or_else(|| format!("assistant entry {} has no round", entry.message_id))?;
                let mut results = Vec::new();
                for (_, id) in calls {
                    let Some(tool) = view.tool(request, round, id)? else {
                        return Ok(messages);
                    };
                    if !tool.is_terminal() {
                        return Ok(messages);
                    }
                    let observation = format!(
                        "{}/observation.json",
                        paths::tool_payload_dir(request.as_str(), round, id)
                    );
                    let observation: Value =
                        serde_json::from_slice(&view.payload(&observation)?)
                            .map_err(|error| format!("parsing {observation}: {error}"))?;
                    results.push(render_tool_observation(view, &tool, observation)?);
                }
                messages.push(message("user", Value::Array(results)));
            }
        }
    }
    Ok(messages)
}

fn render_tool_observation(
    view: &Conversation<'_>,
    tool: &ToolRecord,
    observation: Value,
) -> Result<Value, String> {
    if observation.get("type").and_then(Value::as_str) == Some("tool_result") {
        return Ok(observation);
    }
    if tool.name == subagents::SPAWN_TOOL {
        let parent = view.identity()?.id;
        let child_id = ids::child_id(&parent, &tool.request, tool.round, &tool.id)?;
        let child = view
            .child(&child_id)?
            .ok_or_else(|| format!("spawn tool {} has no child record {child_id}", tool.id))?;
        return Ok(subagents::spawn_result(
            &tool.id,
            &child.id,
            &child.request,
            &child.relay,
        ));
    }
    Ok(result_block(
        &tool.id,
        &observation.to_string(),
        tool.status == ToolStatus::Conflict,
    ))
}

#[allow(clippy::too_many_arguments)]
fn llm_round(
    cfg: &Config,
    state: &mut progress::State,
    request: &Oid,
    request_head: &Oid,
    messages: Vec<Value>,
    workspaces: &[String],
    previous: &Oid,
    round: u64,
) -> Result<(), String> {
    let body = json!({
        "model": cfg.model,
        "max_tokens": MAX_TOKENS,
        "thinking": {"type": "adaptive"},
        "cache_control": {"type": "ephemeral"},
        "system": format!("{}{}", cfg.system, workspaces::context(&state.conversation()?, cfg.focus_workspace.as_deref())?),
        "tools": registry(cfg, workspaces)?,
        "messages": messages,
    });
    let status = |text: &str| eprintln!("llm-step: {text}");
    let mut messages = messages;
    let mut blocks = Vec::new();
    let mut continuation = 0u32;
    let stop = loop {
        if continuation == 0 {
            status(&format!("calling {}…", cfg.model));
        } else {
            status(&format!(
                "{} hit the {MAX_TOKENS}-token cap; continuing ({continuation}/{MAX_CONTINUATIONS})…",
                cfg.model
            ));
        }
        let mut request_body = body.clone();
        request_body["messages"] = Value::Array(messages.clone());
        let started = std::time::Instant::now();
        let response = post_messages(&cfg.base_url, &cfg.api_key, &request_body, &status);
        timing::phase(&format!("model call {}", cfg.model));
        let response = response?;
        status(&format!(
            "{} answered in {:.1}s",
            cfg.model,
            started.elapsed().as_secs_f64()
        ));
        let stop = response["stop_reason"].as_str().unwrap_or("").to_string();
        let round_blocks = response["content"]
            .as_array()
            .cloned()
            .ok_or("API response has no content array")?;
        blocks.extend(round_blocks.iter().cloned());
        if stop == "max_tokens" && continuation < MAX_CONTINUATIONS {
            append_max_tokens_prefill(&mut messages, round_blocks);
            continuation += 1;
        } else {
            break stop;
        }
    };
    let tool_uses: Vec<Value> = blocks
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .cloned()
        .collect();
    let durable = validated_tool_calls(&stop, &tool_uses)?;
    if stop == "max_tokens" {
        return Err(format!(
            "LLM round still hit stop_reason \"max_tokens\" after {MAX_CONTINUATIONS} continuation(s); the response would not converge and the turn fails here"
        ));
    }
    if !matches!(stop.as_str(), "end_turn" | "tool_use") {
        return Err(format!(
            "LLM round ended with stop_reason {stop:?} (only end_turn and tool_use are handled; the turn fails here by design for now)"
        ));
    }

    let (transition, message_id) = model_transition(
        cfg,
        state,
        request,
        round,
        &blocks,
        durable.as_deref().unwrap_or(&[]),
    )?;
    if stop == "end_turn" {
        let ordinal = state.conversation()?.transcript_len()?;
        let result = paths::transcript_entry_path(ordinal, &message_id);
        return terminate_end_turn(
            cfg,
            state,
            EndTurn {
                request,
                request_head,
                expected: previous,
                round,
                model_complete: transition,
                result,
            },
        );
    }
    match state.try_append_at(previous, transition)? {
        progress::TryAppend::Appended(_) => resume(cfg, state, request, request_head),
        progress::TryAppend::HeadChanged(_) => {
            record_response_after_escape(cfg, state, request, request_head, round)
        }
    }
}

fn append_max_tokens_prefill(messages: &mut Vec<Value>, blocks: Vec<Value>) {
    messages.push(message("assistant", Value::Array(blocks)));
}

struct EndTurn<'a> {
    request: &'a Oid,
    request_head: &'a Oid,
    expected: &'a Oid,
    round: u64,
    model_complete: Transition,
    result: String,
}

fn terminate_end_turn(
    cfg: &Config,
    state: &mut progress::State,
    end: EndTurn<'_>,
) -> Result<(), String> {
    let EndTurn {
        request,
        request_head,
        expected,
        round,
        model_complete,
        result,
    } = end;
    state.reload()?;
    if state.head() != expected {
        return record_response_after_escape(cfg, state, request, request_head, round);
    }
    match require_request(&state.conversation()?, request)?.status {
        RequestStatus::Cancelling => return drain(state, request),
        RequestStatus::Idle | RequestStatus::Failed => return finish_from_terminal(state, request),
        RequestStatus::Queued => {
            return Err(format!(
                "request {request} returned to queued after model completion"
            ))
        }
        RequestStatus::Running => {}
    }
    let terminal = Transition::RequestTerminal {
        request: request.clone(),
        outcome: RequestOutcome::Idle {
            result: Some(result),
            interrupted: false,
        },
    };
    match state.try_append_pair_at(expected, model_complete, terminal)? {
        progress::TryAppend::Appended(terminal) => {
            if terminal.ordinal.is_none() {
                return Err("model.complete did not append a transcript entry".to_string());
            }
            reconcile_background_tasks(state)?;
            forward_result(state, &terminal.commit)
        }
        progress::TryAppend::HeadChanged(_) => {
            record_response_after_escape(cfg, state, request, request_head, round)
        }
    }
}

fn model_transition(
    cfg: &Config,
    state: &progress::State,
    request: &Oid,
    round: u64,
    blocks: &[Value],
    calls: &[Value],
) -> Result<(Transition, String), String> {
    let message_id = client_key()?;
    let ordinal = state.conversation()?.transcript_len()?;
    let dir = paths::transcript_payload_dir(ordinal, &message_id);
    let mut payloads = vec![(
        "response.json".to_string(),
        canonical_payload_bytes(&Value::Array(blocks.to_vec()))?,
    )];
    let mut entry_blocks = Vec::new();
    let text = response_text(blocks);
    if !text.is_empty() {
        entry_blocks.push(Block::Text { text });
    }
    let mut declared = Vec::new();
    for call in calls {
        let raw_id = call["id"]
            .as_str()
            .ok_or("durable tool call has no string id")?;
        let name = call["name"]
            .as_str()
            .ok_or("durable tool call has no string name")?;
        let admitted = paths::admit_external_id(raw_id);
        let payload_name = format!("args-{admitted}.json");
        payloads.push((
            payload_name.clone(),
            canonical_payload_bytes(call.get("args").unwrap_or(&Value::Null))?,
        ));
        entry_blocks.push(Block::ToolUse {
            id: raw_id.to_string(),
            name: name.to_string(),
            arguments: format!("{dir}/{payload_name}"),
        });
        declared.push(DeclaredCall {
            id: raw_id.to_string(),
            name: name.to_string(),
        });
    }
    let entry = TranscriptEntry {
        message_id: message_id.clone(),
        conversation: cfg.conversation.clone(),
        role: Role::Assistant,
        actor: cfg.model.clone(),
        request: Some(request.clone()),
        round: Some(round),
        model: Some(cfg.model.clone()),
        blocks: entry_blocks,
        proposal: None,
        workspace_resolution: None,
    };
    Ok((
        Transition::ModelComplete {
            request: request.clone(),
            entry,
            payloads,
            calls: declared,
        },
        message_id,
    ))
}

fn record_response_after_escape(
    cfg: &Config,
    state: &mut progress::State,
    request: &Oid,
    request_head: &Oid,
    round: u64,
) -> Result<(), String> {
    let record = require_request(&state.conversation()?, request)?;
    if record.status == RequestStatus::Cancelling && record.round == round {
        // The shared v3 transition contract currently admits model.complete
        // only while a request is Running. Once request.escape has changed the
        // status to Cancelling there is no legal transition that can retain an
        // assistant response, so finish the required cancellation drain.
        return drain(state, request);
    }
    resume(cfg, state, request, request_head)
}

#[derive(Clone, Debug)]
enum Target {
    Workspace { name: String, commit: Oid },
    Files,
}

fn drive_call(
    cfg: &Config,
    state: &mut progress::State,
    request: &Oid,
    round: &RoundState,
    call: &Call,
) -> Result<bool, String> {
    let site = CallSite::new(request, round, call);
    if let Some(existing) = state
        .conversation()?
        .tool(request, round.declaring_round, &call.id)?
    {
        if existing.status == ToolStatus::Started {
            if existing.name == subagents::WAIT_TOOL {
                dispatch_wait_started(state, request, round.declaring_round, call, &existing)?;
            } else {
                dispatch_started(state, request, round.declaring_round, call, &existing)?;
            }
            return Ok(false);
        }
        return Ok(true);
    }

    if call.name == "workspaces" {
        workspaces::run(state, &site)?;
        return Ok(true);
    }
    if call.name == subagents::SPAWN_TOOL {
        return spawn_agent_call(cfg, state, &site);
    }
    if call.name == subagents::WAIT_TOOL {
        return wait_agent_call(state, &site);
    }
    if call.name == subagents::HARVEST_TOOL {
        harvest_agent_call(state, &site)?;
        return Ok(true);
    }
    if call.name == async_work::TOOL_NAME {
        run_async_call(cfg, state, &site)?;
        return Ok(true);
    }

    let target = match resolve_target(&state.conversation()?, call) {
        Ok(target) => target,
        Err(error) => {
            let block = error_block(&call.id, &error);
            site.failed(state, &block, None)?;
            return Ok(true);
        }
    };

    if tools::is_inline(&call.name) {
        execute_inline(state, &site, target)?;
        return Ok(true);
    }
    let Target::Workspace { name, commit } = target else {
        unreachable!("non-inline code tools cannot target conversation files")
    };
    let (ws, wc) = materialize_workspace(state, &commit)?;
    match prepare_compute(cfg, call, &ws, &wc, request, round)? {
        Prepared::Result(block) => {
            site.complete(state, block, Some((name, commit)), None, None)?;
            Ok(true)
        }
        Prepared::Evaluation => Ok(false),
        Prepared::Task(task) => {
            let record = ToolRecord {
                request: request.clone(),
                round: round.declaring_round,
                id: call.id.clone(),
                name: call.name.clone(),
                declaration_message: round.declaration_message.clone(),
                workspace_name: Some(name),
                input_workspace: Some(commit),
                status: ToolStatus::Started,
                task: Some(task.clone()),
                result: None,
                workspace_resolution: None,
                files: Vec::new(),
                files_outcome: None,
            };
            let expected = state.head().clone();
            match state.try_append_at(
                &expected,
                Transition::ToolStart {
                    record: record.clone(),
                },
            )? {
                progress::TryAppend::HeadChanged(_) => Ok(true),
                progress::TryAppend::Appended(_) => {
                    dispatch_started(state, request, round.declaring_round, call, &record)?;
                    Ok(false)
                }
            }
        }
    }
}

fn resolve_target(view: &Conversation<'_>, call: &Call) -> Result<Target, String> {
    let requested = call
        .input
        .get("workspace")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty());
    // An explicit workspace always owns its paths, including a directory
    // named files. The files/ shorthand applies only without a workspace.
    if requested.is_none() && inline_files_path(call).is_some() {
        return Ok(Target::Files);
    }
    let (name, commit) = workspace_target(view, requested)?
        .ok_or_else(|| "this conversation has no workspace".to_string())?;
    Ok(Target::Workspace { name, commit })
}

fn workspace_target(
    view: &Conversation<'_>,
    requested: Option<&str>,
) -> Result<Option<(String, Oid)>, String> {
    let workspaces = view.workspaces()?;
    let name = match requested {
        Some(name) => name.to_string(),
        None if workspaces.len() == 1 => workspaces.keys().next().unwrap().clone(),
        None if workspaces.is_empty() => return Ok(None),
        None => {
            return Err(format!(
                "several workspaces: {}; pass workspace=<name>",
                workspaces.keys().cloned().collect::<Vec<_>>().join(", ")
            ))
        }
    };
    let Some(workspace) = workspaces.get(&name) else {
        return Err(format!(
            "unknown workspace {name:?}; available workspaces: {}",
            if workspaces.is_empty() {
                "(none)".to_string()
            } else {
                workspaces.keys().cloned().collect::<Vec<_>>().join(", ")
            }
        ));
    };
    Ok(Some((name, workspace.commit.clone())))
}

fn inline_files_path(call: &Call) -> Option<(&'static str, String)> {
    if !tools::is_inline(&call.name) || call.input.get("root").is_some() {
        return None;
    }
    let key = if call.name == "ls" {
        "path"
    } else {
        "file-path"
    };
    let path = call
        .input
        .get(key)?
        .as_str()?
        .trim()
        .trim_start_matches('/');
    path.strip_prefix("files/")
        .map(|relative| (key, relative.to_string()))
}

fn materialize_workspace(
    state: &mut progress::State,
    commit: &Oid,
) -> Result<(String, String), String> {
    static MATERIALIZED: OnceLock<Mutex<HashMap<Oid, (String, String)>>> = OnceLock::new();
    let mut memo = MATERIALIZED
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "workspace materialization cache is poisoned".to_string())?;
    if let Some(paths) = memo.get(commit).cloned() {
        return Ok(paths);
    }
    state.fetch_object(commit)?;
    let tree = state.store().tree_of(commit)?;
    let ws = fresh("workspace");
    caos(["get-hash", tree.as_str(), &ws])?;
    let wc = fresh("workspace-commit");
    caos(["get-hash", commit.as_str(), &wc])?;
    memo.insert(commit.clone(), (ws.clone(), wc.clone()));
    Ok((ws, wc))
}

enum Prepared {
    Result(Value),
    Evaluation,
    Task(Oid),
}

fn prepare_compute(
    cfg: &Config,
    call: &Call,
    ws: &str,
    wc: &str,
    request: &Oid,
    round: &RoundState,
) -> Result<Prepared, String> {
    let clean = call_without_workspace(call);
    match call.name.as_str() {
        "bash" => prepare_bash(cfg, &clean, ws),
        "merge" if cfg.merge_image.is_some() => prepare_merge(cfg, &clean, ws, wc),
        "grep" if cfg.grep_image.is_some() => prepare_grep(cfg, &clean, ws),
        name if std_tool_image(cfg, name).is_some() => prepare_std_tool(cfg, &clean, name, ws),
        name if githist::is_builtin(name) && cfg.tools_image.is_some() => {
            prepare_githist(cfg, &clean, name, ws, wc)
        }
        name if !tools::is_inline(name) => {
            let Some(tool) = tools::tree_tool(ws, name)? else {
                return Err(format!(
                    "model called unknown tool {name:?} (built-ins: bash, grep, read, ls, write, edit, merge, caos-build, caos-test, caos-test-result, spawn_agent, wait_agent, harvest_agent; plus this workspace's caos-tools/<name>/ tools)"
                ));
            };
            match tools::tree_tool_args(&clean, &tool) {
                Err(block) => Ok(Prepared::Result(block)),
                Ok(bound) => {
                    launch_tree_evaluation(
                        &clean,
                        name,
                        &bound,
                        tool.git,
                        ws,
                        wc,
                        request,
                        round.declaring_round,
                    )?;
                    Ok(Prepared::Evaluation)
                }
            }
        }
        name => Err(format!("model called unavailable tool {name:?}")),
    }
}

fn prepare_bash(cfg: &Config, call: &Value, ws: &str) -> Result<Prepared, String> {
    let Some(cmd) = call["input"]["cmd"].as_str() else {
        return Ok(Prepared::Result(error_block(
            call["id"].as_str().unwrap_or(""),
            "bash call has no string `cmd`",
        )));
    };
    let paths: Vec<&str> = match &call["input"]["paths"] {
        Value::Null => Vec::new(),
        Value::Array(items) => match items.iter().map(Value::as_str).collect::<Option<Vec<_>>>() {
            Some(paths) => paths,
            None => {
                return Ok(Prepared::Result(error_block(
                    call["id"].as_str().unwrap_or(""),
                    "bash call `paths` has a non-string entry",
                )))
            }
        },
        _ => {
            return Ok(Prepared::Result(error_block(
                call["id"].as_str().unwrap_or(""),
                "bash call `paths` is not an array",
            )))
        }
    };
    let dir = scratch("toolin")?;
    link(ws, dir.join("tree"))?;
    fs::write(dir.join("cmd"), cmd).map_err(|error| format!("writing cmd: {error}"))?;
    fs::write(dir.join("paths"), paths.join("\n"))
        .map_err(|error| format!("writing paths: {error}"))?;
    let input = fresh("toolin");
    caos(["put", path(&dir), &input])?;
    prepared_request(&cfg.bash_image, &[], &input)
}

fn prepare_merge(cfg: &Config, call: &Value, ws: &str, wc: &str) -> Result<Prepared, String> {
    let theirs = match resolve_theirs(cfg, call) {
        Ok(theirs) => theirs,
        Err(block) => return Ok(Prepared::Result(block)),
    };
    let theirs_path = fresh("theirs");
    caos(["get-hash", &theirs, &theirs_path])?;
    let image = cfg.merge_image.as_deref().ok_or("merge image is absent")?;
    let curried = caos_curry(
        Arg::Hash(image),
        &[("ours", Arg::Path(wc)), ("theirs", Arg::Path(&theirs_path))],
    )?;
    prepared_request(&curried, &[], ws)
}

fn prepare_grep(cfg: &Config, call: &Value, ws: &str) -> Result<Prepared, String> {
    let (scope, _) = match tools::grep_precheck(call, ws) {
        Ok(scope) => scope,
        Err(block) => return Ok(Prepared::Result(block)),
    };
    let pattern = call["input"]["pattern"]
        .as_str()
        .ok_or("grep precheck admitted no string pattern")?;
    let image = cfg.grep_image.as_deref().ok_or("grep image is absent")?;
    let curried = caos_curry(Arg::Hash(image), &[("pattern", Arg::Lit(pattern))])?;
    prepared_request(&curried, &[], &scope)
}

fn prepare_std_tool(cfg: &Config, call: &Value, name: &str, ws: &str) -> Result<Prepared, String> {
    let (image, arg_name) = std_tool_image(cfg, name).ok_or("std tool image is absent")?;
    let tool = tools::std_tool(name, &arg(arg_name))?
        .ok_or_else(|| format!("{name} image carries no help"))?;
    let bound = match tools::tree_tool_args(call, &tool) {
        Ok(bound) => bound,
        Err(block) => return Ok(Prepared::Result(block)),
    };
    let args: Vec<(&str, Arg<'_>)> = bound
        .iter()
        .map(|(name, value)| (name.as_str(), Arg::Lit(value)))
        .collect();
    let curried = caos_curry(Arg::Hash(image), &args)?;
    prepared_request(&curried, &[], ws)
}

fn prepare_githist(
    cfg: &Config,
    call: &Value,
    name: &str,
    ws: &str,
    wc: &str,
) -> Result<Prepared, String> {
    let tool = githist::tool(name).ok_or_else(|| format!("no built-in tool {name}"))?;
    let bound = match tools::tree_tool_args(call, &tool) {
        Ok(bound) => bound,
        Err(block) => return Ok(Prepared::Result(block)),
    };
    let body = githist::script(name).ok_or_else(|| format!("no built-in script for {name}"))?;
    let dir = scratch(&format!("githist-{name}"))?;
    let file = dir.join("worker.sh");
    fs::write(&file, body).map_err(|error| format!("writing {name} script: {error}"))?;
    let script = fresh("githist-script");
    caos(["put", path(&file), &script])?;
    let image = cfg.tools_image.as_deref().ok_or("tools image is absent")?;
    let mut args: Vec<(&str, Arg<'_>)> = vec![("worker1", Arg::Path(&script))];
    args.extend(
        bound
            .iter()
            .map(|(name, value)| (name.as_str(), Arg::Lit(value))),
    );
    args.push(("wc", Arg::Path(wc)));
    if let Some(refs) = cfg.merge_refs.as_deref() {
        args.push(("refs", Arg::Lit(refs)));
    }
    let curried = caos_curry(Arg::Hash(image), &args)?;
    prepared_request(&curried, &[], ws)
}

fn prepared_request(
    image: &str,
    args: &[(&str, Arg<'_>)],
    input: &str,
) -> Result<Prepared, String> {
    let curried = if args.is_empty() {
        image.to_string()
    } else {
        caos_curry(Arg::Hash(image), args)?
    };
    let task = prepare_request(Arg::Hash(&curried), &[("in", Arg::Path(input))])?;
    Ok(Prepared::Task(Oid::parse(&task, "tool task")?))
}

#[allow(clippy::too_many_arguments)]
fn launch_tree_evaluation(
    call: &Value,
    name: &str,
    bound: &[(String, String)],
    git: bool,
    ws: &str,
    wc: &str,
    request: &Oid,
    round: u64,
) -> Result<(), String> {
    let id = call["id"]
        .as_str()
        .ok_or("tool_use block has no string id")?;
    let args: serde_json::Map<String, Value> = bound
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();
    let serialized = Value::Object(args).to_string();
    let me = self_curry(
        Some(wc),
        request,
        round,
        id,
        &[
            ("current-tool", Arg::Lit(name)),
            ("ws", Arg::Path(ws)),
            ("tool-eval", Arg::Lit(name)),
            ("tool-args", Arg::Lit(&serialized)),
            ("tool-git", Arg::Lit(if git { "1" } else { "" })),
        ],
    )?;
    let dispatched = eval_then_catching(ws, &format!("caos-tools/{name}"), Arg::Hash(&me));
    timing::phase(&format!("tool dispatch {name}"));
    dispatched
}

fn launch_evaluated_tool(
    cfg: &Config,
    state: &mut progress::State,
    request: &Oid,
    request_head: &Oid,
    round: u64,
    id: &str,
    name: &str,
) -> Result<(), String> {
    let record = require_request(&state.conversation()?, request)?;
    let current = round_state(&state.conversation()?, &record)?;
    if current.declaring_round != round {
        return resume(cfg, state, request, request_head);
    }
    let call = current
        .pending
        .iter()
        .find(|call| call.id == id)
        .cloned()
        .ok_or_else(|| format!("evaluated tool {id} is no longer pending"))?;
    let target = resolve_target(&state.conversation()?, &call)?;
    let Target::Workspace {
        name: workspace_name,
        commit,
    } = target
    else {
        return Err("tree tool unexpectedly targeted conversation files".to_string());
    };
    let (ws, wc) = materialize_workspace(state, &commit)?;
    let tool_tree = cas_hash(&arg("result"))?;
    let raw = read_arg("tool-args")?;
    let parsed: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("re-reading the tool's args: {error}"))?;
    let bound: Vec<(String, String)> = parsed
        .as_object()
        .ok_or("tool-args is not a JSON object")?
        .iter()
        .map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_string()))
        .collect();
    let git = read_arg_opt("tool-git")?.is_some_and(|value| value == "1");
    let mut args: Vec<(&str, Arg<'_>)> = bound
        .iter()
        .map(|(key, value)| (key.as_str(), Arg::Lit(value)))
        .collect();
    if git {
        args.push(("wc", Arg::Path(&wc)));
        if let Some(refs) = cfg.merge_refs.as_deref() {
            args.push(("refs", Arg::Lit(refs)));
        }
    }
    let curried = caos_curry(Arg::Hash(&tool_tree), &args)?;
    let task_text = prepare_request(Arg::Hash(&curried), &[("in", Arg::Path(&ws))])?;
    let task = Oid::parse(&task_text, "tree tool task")?;
    let started = ToolRecord {
        request: request.clone(),
        round,
        id: id.to_string(),
        name: name.to_string(),
        declaration_message: current.declaration_message,
        workspace_name: Some(workspace_name),
        input_workspace: Some(commit),
        status: ToolStatus::Started,
        task: Some(task),
        result: None,
        workspace_resolution: None,
        files: Vec::new(),
        files_outcome: None,
    };
    let expected = state.head().clone();
    match state.try_append_at(
        &expected,
        Transition::ToolStart {
            record: started.clone(),
        },
    )? {
        progress::TryAppend::HeadChanged(_) => resume(cfg, state, request, request_head),
        progress::TryAppend::Appended(_) => {
            dispatch_started(state, request, round, &call, &started)
        }
    }
}

fn dispatch_started(
    state: &mut progress::State,
    request: &Oid,
    round: u64,
    call: &Call,
    record: &ToolRecord,
) -> Result<(), String> {
    let commit = record
        .input_workspace
        .as_ref()
        .ok_or("compute tool.start has no input workspace")?;
    let (ws, wc) = materialize_workspace(state, commit)?;
    let mut extras = vec![
        ("current-tool", Arg::Lit(call.name.as_str())),
        ("ws", Arg::Path(ws.as_str())),
    ];
    let clean = call_without_workspace(call);
    let scope_storage;
    if call.name == "grep" {
        let (_, prefix) = tools::grep_precheck(&clean, &ws)
            .map_err(|_| "recorded grep task no longer passes its precheck".to_string())?;
        scope_storage = prefix;
        extras.push(("scope", Arg::Lit(&scope_storage)));
    }
    let me = self_curry(Some(&wc), request, round, &call.id, &extras)?;
    let task = record.task.as_ref().ok_or("started tool has no task")?;
    let dispatched = run_request_then_catching(task.as_str(), Arg::Hash(&me));
    timing::phase(&format!("tool dispatch {}", call.name));
    dispatched
        .map_err(|error| format!("launching recorded task {task} for {}: {error}", call.name))?;
    Ok(())
}

fn dispatch_wait_started(
    state: &mut progress::State,
    request: &Oid,
    round: u64,
    call: &Call,
    record: &ToolRecord,
) -> Result<(), String> {
    if record.workspace_name.is_some() || record.input_workspace.is_some() {
        return Err("wait_agent tool.start unexpectedly names a workspace".to_string());
    }
    let value = call.value();
    let child_id = subagents::required_string(&value, "child", subagents::WAIT_TOOL)?;
    let child = state
        .conversation()?
        .child(child_id)?
        .ok_or_else(|| format!("wait_agent child {child_id:?} disappeared"))?;
    let relay = record
        .task
        .as_ref()
        .ok_or("wait_agent tool.start has no relay task")?;
    if child.relay != *relay {
        return Err(format!(
            "wait_agent child {child_id:?} records relay {}, not {relay}",
            child.relay
        ));
    }
    validate_child_relay(state, &child)?;
    let extras = [("current-tool", Arg::Lit(call.name.as_str()))];
    let me = self_curry(None, request, round, &call.id, &extras)?;
    let dispatched = run_request_then_catching(relay.as_str(), Arg::Hash(&me));
    timing::phase(&format!("tool dispatch {}", call.name));
    dispatched.map_err(|error| format!("joining subagent relay {relay}: {error}"))
}

fn callback_result(
    state: &mut progress::State,
    record: &ToolRecord,
) -> Result<(Value, Option<Oid>), String> {
    match record.name.as_str() {
        subagents::WAIT_TOOL => wait_callback_block(state, record),
        "grep" => {
            let scope = read_arg_opt("scope")?.unwrap_or_default();
            Ok((
                tools::grep_result_block(&record.id, &arg("result"), &scope)?,
                None,
            ))
        }
        "merge" => {
            let proposal = Oid::parse(&cas_hash(&arg("result"))?, "merge result commit")?;
            state.fetch_object(&proposal)?;
            let tree = state.store().tree_of(&proposal)?;
            let ws = fresh("merge-workspace");
            caos(["get-hash", tree.as_str(), &ws])?;
            Ok((merge_result_block(&record.id, &ws)?, Some(proposal)))
        }
        "bash" => {
            let block = bash_result_block(&record.id)?;
            let ws = format!("{}/tree", arg("result"));
            if !Path::new(&ws).exists() {
                return Err("bash result carries no `tree` entry".to_string());
            }
            caos(["get", &ws])?;
            let tree = Oid::parse(&cas_hash(&ws)?, "bash result tree")?;
            let base = record
                .input_workspace
                .as_ref()
                .ok_or("bash record has no input workspace")?;
            let proposal = mint_workspace_commit(state, &tree, base, "bash")?;
            Ok((block, Some(proposal)))
        }
        _ => Ok((
            tools::tree_tool_result_block(&record.id, &arg("result"))?,
            None,
        )),
    }
}

fn mint_workspace_commit(
    state: &mut progress::State,
    tree: &Oid,
    parent: &Oid,
    message: &str,
) -> Result<Oid, String> {
    state.fetch_object(tree)?;
    let signature = inherited_signature(state.store(), parent)?;
    let commit = state.store_mut().commit(
        tree,
        std::slice::from_ref(parent),
        &format!("{message}\n"),
        &signature,
    )?;
    Ok(commit)
}

fn complete_compute(
    state: &mut progress::State,
    started: &ToolRecord,
    base_block: Value,
    proposal: Option<Oid>,
) -> Result<(), String> {
    let Some(proposal) = proposal else {
        let record = completed_record(
            started,
            ToolResult::Complete {
                observation: observation_path(started),
                proposal: None,
            },
            None,
        );
        state.append(tool_complete_transition(record, &base_block, Vec::new())?)?;
        return Ok(());
    };
    state.publish_commit(&proposal)?;
    for _ in 0..32 {
        state.reload()?;
        if state
            .conversation()?
            .tool(&started.request, started.round, &started.id)?
            .is_some_and(|record| record.is_terminal())
        {
            return Ok(());
        }
        let current = started
            .workspace_name
            .as_ref()
            .map(|name| state.conversation()?.workspace(name))
            .transpose()?
            .flatten()
            .map(|workspace| workspace.commit);
        let base = started
            .input_workspace
            .as_ref()
            .ok_or("proposal tool has no input workspace")?;
        let signature = inherited_signature(state.store(), base)?;
        let resolution = reconcile(
            state.store_mut(),
            base,
            &proposal,
            current.as_ref(),
            &signature,
        )?;
        if let WorkspaceResolution::Merged { output, .. } = &resolution {
            state.push_code(output)?;
        }
        let block = if matches!(resolution, WorkspaceResolution::Conflict { .. }) {
            error_block(
                &started.id,
                &format!(
                    "workspace proposal for call {} conflicted with concurrent changes; the workspace is unchanged and proposal {} was retained",
                    started.id, proposal
                ),
            )
        } else {
            base_block.clone()
        };
        let record = completed_record(
            started,
            ToolResult::Complete {
                observation: observation_path(started),
                proposal: Some(proposal.clone()),
            },
            Some(resolution),
        );
        let expected = state.head().clone();
        match state.try_append_at(
            &expected,
            tool_complete_transition(record, &block, Vec::new())?,
        )? {
            progress::TryAppend::Appended(_) => return Ok(()),
            progress::TryAppend::HeadChanged(_) => continue,
        }
    }
    Err(format!(
        "conversation kept changing while reconciling call {}",
        started.id
    ))
}

fn completed_record(
    started: &ToolRecord,
    result: ToolResult,
    resolution: Option<WorkspaceResolution>,
) -> ToolRecord {
    ToolRecord {
        status: ToolRecord::expected_status(&result, resolution.as_ref()),
        result: Some(result),
        workspace_resolution: resolution,
        files: Vec::new(),
        files_outcome: None,
        ..started.clone()
    }
}

fn complete_started_failed(
    state: &mut progress::State,
    started: &ToolRecord,
    block: &Value,
) -> Result<(), String> {
    let record = completed_record(
        started,
        ToolResult::Failed {
            error: observation_path(started),
        },
        None,
    );
    state.append(tool_complete_transition(record, block, Vec::new())?)?;
    Ok(())
}

fn declaration_message(
    view: &Conversation<'_>,
    request: &Oid,
    round: u64,
) -> Result<String, String> {
    let record = require_request(view, request)?;
    let state = round_state(view, &record)?;
    if state.declaring_round != round {
        return Err(format!(
            "request {request} no longer has declaring round {round}"
        ));
    }
    Ok(state.declaration_message)
}

fn observation_path(record: &ToolRecord) -> String {
    format!(
        "{}/observation.json",
        paths::tool_payload_dir(record.request.as_str(), record.round, &record.id)
    )
}

#[allow(clippy::type_complexity)]
fn tool_complete_transition(
    record: ToolRecord,
    block: &Value,
    files: Vec<(String, Option<(Mode, Vec<u8>)>)>,
) -> Result<Transition, String> {
    Ok(Transition::ToolComplete {
        record,
        payloads: vec![(
            "observation.json".to_string(),
            canonical_payload_bytes(block)?,
        )],
        files,
    })
}

fn execute_inline(
    state: &mut progress::State,
    site: &CallSite<'_>,
    target: Target,
) -> Result<(), String> {
    let call = site.call;
    match target {
        Target::Workspace { name, commit } => {
            let (ws, _) = materialize_workspace(state, &commit)?;
            let clean = call_without_workspace(call);
            let (block, new_ws) = tools::execute(&clean, &ws)?;
            let (proposal, resolution) = match new_ws {
                None => (None, None),
                Some(new_ws) => {
                    let tree = Oid::parse(&cas_hash(&new_ws)?, "inline result tree")?;
                    let proposal = mint_workspace_commit(state, &tree, &commit, &call.name)?;
                    // Inline mutation already published this tree through the
                    // `caos put` in tools::rebuild.
                    state.publish_commit(&proposal)?;
                    (
                        Some(proposal.clone()),
                        Some(WorkspaceResolution::Direct {
                            current: commit.clone(),
                            output: proposal,
                        }),
                    )
                }
            };
            site.complete(state, block, Some((name, commit)), proposal, resolution)?;
        }
        Target::Files => {
            let (key, relative) =
                inline_files_path(call).ok_or("conversation-file call has no files/ path")?;
            let root = materialize_files(state)?;
            let mut clean = call_without_workspace(call);
            clean["input"][key] = Value::String(relative.clone());
            let (block, new_root) = tools::execute(&clean, &root)?;
            let (files, outcome) = match new_root {
                None => (Vec::new(), None),
                Some(root) => {
                    let file = materialize_relative(&root, &relative)?;
                    let bytes = fs::read(&file)
                        .map_err(|error| format!("reading {}: {error}", file.display()))?;
                    let mode = fs::metadata(&file)
                        .map_err(|error| format!("stat {}: {error}", file.display()))?
                        .permissions()
                        .mode();
                    let mode = if mode & 0o111 != 0 {
                        Mode::Executable
                    } else {
                        Mode::Blob
                    };
                    (
                        vec![(relative.clone(), Some((mode, bytes)))],
                        Some(FilesOutcome {
                            applied: vec![relative.clone()],
                            conflicted: Vec::new(),
                        }),
                    )
                }
            };
            let stub = site.stub(None);
            let result = ToolResult::Complete {
                observation: observation_path(&stub),
                proposal: None,
            };
            let mut record = completed_record(&stub, result, None);
            record.files = files.iter().map(|(path, _)| path.clone()).collect();
            record.files_outcome = outcome;
            let transition = tool_complete_transition(record, &block, files)?;
            state.append(transition)?;
        }
    }
    Ok(())
}

fn materialize_files(state: &mut progress::State) -> Result<String, String> {
    let files = state
        .conversation()?
        .snapshot()
        .entry(paths::FILES_DIR)?
        .map(|entry| entry.oid);
    let output = fresh("conversation-files");
    match files {
        Some(tree) => caos(["get-hash", tree.as_str(), &output])?,
        None => {
            let empty = scratch("conversation-files-empty")?;
            caos(["put", path(&empty), &output])?;
        }
    }
    Ok(output)
}

fn materialize_relative(root: &str, relative: &str) -> Result<std::path::PathBuf, String> {
    let mut current = std::path::PathBuf::from(root);
    for component in relative.split('/') {
        let _ = caos(["get", path(&current)]);
        current.push(component);
    }
    caos(["get", path(&current)])?;
    Ok(current)
}

fn call_without_workspace(call: &Call) -> Value {
    let mut input = call.input.clone();
    if let Some(object) = input.as_object_mut() {
        object.remove("workspace");
    }
    json!({"type":"tool_use", "id":call.id, "name":call.name, "input":input})
}

fn spawn_agent_call(
    cfg: &Config,
    state: &mut progress::State,
    site: &CallSite<'_>,
) -> Result<bool, String> {
    let request = site.request;
    let call = site.call;
    let prompt = match subagents::required_string(&call.value(), "prompt", subagents::SPAWN_TOOL) {
        Ok(prompt) => prompt.to_string(),
        Err(error) => {
            site.fail(state, &error)?;
            return Ok(true);
        }
    };
    let value = call.value();
    let requested_workspace = subagents::optional_string(&value, "workspace");
    let target = match workspace_target(&state.conversation()?, requested_workspace) {
        Ok(target) => target,
        Err(error) => {
            site.fail(state, &error)?;
            return Ok(true);
        }
    };
    let run_and_update_ref_image = cfg
        .run_and_update_ref_image
        .as_deref()
        .ok_or("spawn_agent was called without a run-and-update-ref image")?;
    let parent_head = state.head().clone();
    let parent_view = state.conversation()?;
    let parent_id = parent_view.identity()?.id;
    let actor = newest_user_actor(&parent_view)?;
    let child_config = target
        .as_ref()
        .map(|(name, _)| parent_view.workspace_config(name))
        .transpose()?
        .map(|mut config| {
            config.branch = None;
            if matches!(
                config.base,
                Some(conversation_protocol::v3::WorkspaceBase::Workspace { .. })
            ) {
                config.base = None;
            }
            config
        });
    let prompt_path = tool_arguments_path(&parent_view, request, site.round, &call.id)?;
    drop(parent_view);
    let child_id = ids::child_id(&parent_id, request, site.round, &call.id)?;
    let signature = inherited_signature(state.store(), &parent_head)?;
    let genesis = conversation_protocol::v3::oid::ensure_genesis(state.store_mut())?;
    let workspaces = target
        .clone()
        .into_iter()
        .map(|(name, commit)| (name, (commit, None)))
        .collect::<BTreeMap<_, _>>();
    let root_transition = Transition::ConversationRoot {
        identity: Identity {
            id: child_id.clone(),
            kind: IdentityKind::Root,
            owner: Some(Owner {
                parent: parent_id,
                parent_head: parent_head.clone(),
                request: request.clone(),
                round: site.round,
                tool: call.id.clone(),
            }),
        },
        title: subagents::agent_title(&prompt),
        workspaces,
        files_seed: None,
    };
    let root_tree = apply(state.store_mut(), None, &root_transition)?.tree;
    let root = mint(
        state.store_mut(),
        &genesis,
        &root_tree,
        root_transition.kind(),
        &signature,
    )?;
    let root = if let (Some((name, _)), Some(config)) = (
        target.as_ref(),
        child_config.filter(|config| config != &Default::default()),
    ) {
        mint_detached(
            state,
            &root,
            &Transition::WorkspaceConfigure {
                name: name.clone(),
                config,
            },
            &signature,
        )?
    } else {
        root
    };
    let prompt_message = ids::protocol_id("subagent-prompt", &json!({"child": child_id.as_str()}))?;
    let prompt_transition = Transition::MessageAppend {
        entry: TranscriptEntry {
            message_id: prompt_message,
            conversation: child_id.clone(),
            role: Role::User,
            actor,
            request: None,
            round: None,
            model: None,
            blocks: vec![Block::Text {
                text: prompt.clone(),
            }],
            proposal: None,
            workspace_resolution: None,
        },
        payloads: Vec::new(),
    };
    let prompt_head = mint_detached(state, &root, &prompt_transition, &signature)?;
    state.push_code(&prompt_head)?;
    let (configuration, child_request) =
        subagents::child_request(&child_id, &prompt_head, &cfg.system)?;
    let request_workspaces = state.conversation_at(&prompt_head)?.workspaces_tree()?;
    let admit = Transition::RequestAdmit {
        record: RequestRecord {
            id: child_request.clone(),
            request_head: prompt_head.clone(),
            request_workspaces,
            model: cfg.model.clone(),
            configuration: configuration.to_string(),
            round: 0,
            calls: Vec::new(),
            interjections: Vec::new(),
            status: RequestStatus::Queued,
            latest_message: None,
            escape_reason: None,
            outcome: None,
        },
    };
    let initial_head = mint_detached(state, &prompt_head, &admit, &signature)?;
    validate_spine(state.store(), &initial_head, &mut HashSet::new()).map_err(String::from)?;
    let relay = subagents::prepare_relay(
        &child_request,
        state.refname(),
        &child_id,
        run_and_update_ref_image,
    )?;
    let observation = subagents::spawn_observation(&child_id, &initial_head, &child_request);
    let stub = site.stub(target.clone());
    let tool = completed_record(
        &stub,
        ToolResult::Complete {
            observation: observation_path(&stub),
            proposal: None,
        },
        None,
    );
    let child = ChildRecord {
        id: child_id.clone(),
        initial_head: initial_head.clone(),
        initial_workspace: target.as_ref().map(|(_, commit)| commit.clone()),
        request: child_request.clone(),
        relay: relay.clone(),
        spawn_intent: SpawnIntent {
            request: request.clone(),
            round: site.round,
            tool: call.id.clone(),
            workspace_name: target.as_ref().map(|(name, _)| name.clone()),
            input_workspace: target.as_ref().map(|(_, commit)| commit.clone()),
            prompt: prompt_path,
            model: cfg.model.clone(),
            configuration: configuration.to_string(),
            files_seed: None,
        },
        status: ChildStatus::Running,
        applications: Vec::new(),
        terminal_head: None,
        child_workspaces: None,
    };
    let transition = Transition::SubagentSpawn {
        tool,
        payloads: vec![(
            "observation.json".to_string(),
            canonical_payload_bytes(&observation)?,
        )],
        child: child.clone(),
    };
    match state.atomic_spawn(&parent_head, transition, &child)? {
        progress::TryAppend::HeadChanged(_) => Ok(true),
        progress::TryAppend::Appended(_) => {
            let dispatched = subagents::dispatch(&relay);
            timing::phase("tool dispatch spawn_agent");
            if let Err(error) = dispatched {
                eprintln!(
                    "llm-step: could not dispatch subagent {child_id} relay {relay}: {error}; recovery will retry"
                );
            }
            Ok(true)
        }
    }
}

fn mint_detached(
    state: &mut progress::State,
    parent: &Oid,
    transition: &Transition,
    signature: &conversation_protocol::v3::Signature,
) -> Result<Oid, String> {
    let parent_tree = state
        .store()
        .read_commit(parent)
        .map_err(String::from)?
        .tree;
    let tree = apply(state.store_mut(), Some(&parent_tree), transition)?.tree;
    mint(
        state.store_mut(),
        parent,
        &tree,
        transition.kind(),
        signature,
    )
}

fn newest_user_actor(view: &Conversation<'_>) -> Result<String, String> {
    last_matching(view, |_, entry| {
        (entry.role == Role::User).then_some(entry.actor)
    })?
    .ok_or_else(|| "request has no user actor in the transcript".to_string())
}

fn tool_arguments_path(
    view: &Conversation<'_>,
    request: &Oid,
    round: u64,
    tool: &str,
) -> Result<String, String> {
    last_matching(view, |_, entry| {
        if entry.role != Role::Assistant
            || entry.request.as_ref() != Some(request)
            || entry.round != Some(round)
        {
            return None;
        }
        entry.blocks.into_iter().find_map(|block| {
            if let Block::ToolUse { id, arguments, .. } = block {
                if id == tool {
                    return Some(arguments);
                }
            }
            None
        })
    })?
    .ok_or_else(|| format!("declared tool {request}/{round}/{tool} has no argument payload"))
}

fn wait_agent_call(state: &mut progress::State, site: &CallSite<'_>) -> Result<bool, String> {
    let call = site.call;
    let child_id = match subagents::required_string(&call.value(), "child", subagents::WAIT_TOOL) {
        Ok(child) => child.to_string(),
        Err(error) => {
            site.fail(state, &error)?;
            return Ok(true);
        }
    };
    let Some(child) = state.conversation()?.child(&child_id)? else {
        site.fail(state, &format!("unknown subagent {child_id}"))?;
        return Ok(true);
    };
    validate_child_relay(state, &child)?;
    let started = ToolRecord {
        request: site.request.clone(),
        round: site.round,
        id: call.id.clone(),
        name: call.name.clone(),
        declaration_message: site.declaration.to_string(),
        workspace_name: None,
        input_workspace: None,
        status: ToolStatus::Started,
        task: Some(child.relay.clone()),
        result: None,
        workspace_resolution: None,
        files: Vec::new(),
        files_outcome: None,
    };
    let expected = state.head().clone();
    match state.try_append_at(
        &expected,
        Transition::ToolStart {
            record: started.clone(),
        },
    )? {
        progress::TryAppend::HeadChanged(_) => Ok(true),
        progress::TryAppend::Appended(_) => {
            dispatch_wait_started(state, site.request, site.round, call, &started)?;
            Ok(false)
        }
    }
}

fn validate_child_relay(state: &progress::State, child: &ChildRecord) -> Result<(), String> {
    let (request, target_ref, relay_child) = subagents::relay_request(&child.relay)?;
    if request != child.request || target_ref != state.refname() || relay_child != child.id {
        return Err(format!(
            "subagent {} relay {} does not match its recorded request and parent ref",
            child.id, child.relay
        ));
    }
    Ok(())
}

fn wait_callback_block(
    state: &mut progress::State,
    record: &ToolRecord,
) -> Result<(Value, Option<Oid>), String> {
    let relay = record
        .task
        .as_ref()
        .ok_or("wait_agent callback has no relay task")?;
    let (_, target_ref, child_id) = subagents::relay_request(relay)?;
    if target_ref != state.refname() {
        return Err(format!(
            "wait_agent relay {relay} targets {target_ref}, not {}",
            state.refname()
        ));
    }
    let child = state
        .conversation()?
        .child(&child_id)?
        .ok_or_else(|| format!("subagent {child_id} disappeared"))?;
    if child.status == ChildStatus::Running {
        return Ok((
            error_block(
                &record.id,
                &format!("subagent {child_id} has not reported yet"),
            ),
            None,
        ));
    }
    let terminal_head = child
        .terminal_head
        .as_ref()
        .ok_or_else(|| format!("terminal subagent {child_id} has no terminal_head"))?;
    let workspaces = child
        .child_workspaces
        .as_ref()
        .ok_or_else(|| format!("terminal subagent {child_id} has no child_workspaces"))?
        .iter()
        .map(|(name, workspace)| (name.clone(), Value::String(workspace.commit.to_string())))
        .collect::<serde_json::Map<_, _>>();
    Ok((
        json!({
            "child": child_id,
            "status": child_status_text(child.status),
            "terminal_head": terminal_head.as_str(),
            "workspaces": Value::Object(workspaces),
        }),
        None,
    ))
}

fn harvest_agent_call(state: &mut progress::State, site: &CallSite<'_>) -> Result<(), String> {
    let request = site.request;
    let call = site.call;
    let child_id = match subagents::required_string(&call.value(), "child", subagents::HARVEST_TOOL)
    {
        Ok(child) => child.to_string(),
        Err(error) => return site.fail(state, &error),
    };
    for _ in 0..32 {
        state.reload()?;
        if state
            .conversation()?
            .tool(request, site.round, &call.id)?
            .is_some_and(|record| record.is_terminal())
        {
            return Ok(());
        }
        let Some(child) = state.conversation()?.child(&child_id)? else {
            return site.fail(state, &format!("unknown subagent {child_id}"));
        };
        if child.status == ChildStatus::Running {
            return site.fail(state, &format!("subagent {child_id} is still running"));
        }
        let child_workspace = subagents::optional_string(&call.value(), "child_workspace")
            .map(str::to_string)
            .or_else(|| child.spawn_intent.workspace_name.clone());
        let Some(child_workspace) = child_workspace else {
            return site.fail(
                state,
                "harvest_agent needs `child_workspace` because this child was seeded without one",
            );
        };
        let child_state = child
            .child_workspaces
            .as_ref()
            .and_then(|workspaces| workspaces.get(&child_workspace))
            .cloned();
        let Some(child_state) = child_state else {
            return site.fail(
                state,
                &format!("subagent {child_id} has no workspace {child_workspace:?}"),
            );
        };
        let parent_workspace = match workspace_target(
            &state.conversation()?,
            subagents::optional_string(&call.value(), "workspace"),
        ) {
            Ok(Some(workspace)) => workspace,
            Ok(None) => return site.fail(state, "this conversation has no workspace"),
            Err(error) => return site.fail(state, &error),
        };
        if let Some(application) = recover_harvest_application(
            state,
            &child_id,
            &child_workspace,
            &parent_workspace.0,
            site.declaration,
        )? {
            return complete_harvest(state, site, &child_state.commit, &application);
        }

        for oid in [
            &child_state.initial,
            &child_state.commit,
            &parent_workspace.1,
        ] {
            state.fetch_object(oid)?;
        }
        let signature = inherited_signature(state.store(), &parent_workspace.1)?;
        let resolution = reconcile(
            state.store_mut(),
            &child_state.initial,
            &child_state.commit,
            Some(&parent_workspace.1),
            &signature,
        )?;
        if let WorkspaceResolution::Merged { output, .. } = &resolution {
            state.push_code(output)?;
        }
        let application = Application {
            parent_workspace_name: parent_workspace.0,
            parent_workspace: Some(parent_workspace.1),
            child_workspace,
            workspace_resolution: resolution,
        };
        let expected = state.head().clone();
        match state.try_append_at(
            &expected,
            Transition::SubagentApply {
                child: child_id.clone(),
                application: application.clone(),
            },
        )? {
            progress::TryAppend::HeadChanged(_) => continue,
            progress::TryAppend::Appended(_) => {
                return complete_harvest(state, site, &child_state.commit, &application)
            }
        }
    }
    Err("conversation kept changing while applying a subagent workspace".to_string())
}

fn complete_harvest(
    state: &mut progress::State,
    site: &CallSite<'_>,
    child_workspace: &Oid,
    application: &Application,
) -> Result<(), String> {
    let conflict = matches!(
        application.workspace_resolution,
        WorkspaceResolution::Conflict { .. }
    );
    let target = conflict.then(|| {
        (
            application.parent_workspace_name.clone(),
            application
                .parent_workspace
                .clone()
                .expect("subagent applications always name a parent workspace"),
        )
    });
    site.complete(
        state,
        application.workspace_resolution.to_value(),
        target,
        conflict.then(|| child_workspace.clone()),
        conflict.then(|| application.workspace_resolution.clone()),
    )
}

fn recover_harvest_application(
    state: &progress::State,
    child: &str,
    child_workspace: &str,
    parent_workspace: &str,
    declaration_message: &str,
) -> Result<Option<Application>, String> {
    let current_pointer = state
        .conversation()?
        .workspace(parent_workspace)?
        .ok_or_else(|| format!("workspace {parent_workspace:?} disappeared"))?
        .commit;
    let mut current = state.head().clone();
    for _ in 0..MAX_SPINE_WALK {
        let info = state.store().read_commit(&current).map_err(String::from)?;
        let parent = info
            .parents
            .first()
            .ok_or_else(|| format!("conversation commit {current} has no parent"))?;
        let after = state.conversation_at(&current)?;
        let before = state.conversation_at(parent)?;
        if Kind::parse_message(&info.message)? == Kind::SubagentApply {
            let after_record = after.child(child)?;
            let before_record = before.child(child)?;
            if let (Some(after_record), Some(before_record)) = (after_record, before_record) {
                if after_record.applications.len() == before_record.applications.len() + 1
                    && after_record
                        .applications
                        .starts_with(&before_record.applications)
                {
                    let application = after_record.applications.last().unwrap();
                    let visible = application.workspace_resolution.new_pointer().map_or_else(
                        || application.parent_workspace.as_ref() == Some(&current_pointer),
                        |output| output == &current_pointer,
                    );
                    if application.child_workspace == child_workspace
                        && application.parent_workspace_name == parent_workspace
                        && visible
                    {
                        return Ok(Some(application.clone()));
                    }
                }
            }
        }
        let declaration_added_here = transcript_has_message(&after, declaration_message)?
            && !transcript_has_message(&before, declaration_message)?;
        if declaration_added_here {
            return Ok(None);
        }
        current = parent.clone();
    }
    Err(format!(
        "harvest recovery walk exceeded {MAX_SPINE_WALK} commits"
    ))
}

fn message_ordinal(view: &Conversation<'_>, message_id: &str) -> Result<Option<u64>, String> {
    for ordinal in 0..view.transcript_len()? {
        if view
            .transcript_entry(ordinal)?
            .is_some_and(|(_, entry)| entry.message_id == message_id)
        {
            return Ok(Some(ordinal));
        }
    }
    Ok(None)
}

fn transcript_has_message(view: &Conversation<'_>, message_id: &str) -> Result<bool, String> {
    message_ordinal(view, message_id).map(|ordinal| ordinal.is_some())
}

fn run_async_call(
    cfg: &Config,
    state: &mut progress::State,
    site: &CallSite<'_>,
) -> Result<(), String> {
    let call = site.call;
    let value = call.value();
    let subrequest = match async_work::request(&value) {
        Ok(request) => request,
        Err(block) => return site.complete(state, block, None, None, None),
    };
    if let Some(error) = async_request_error(subrequest)? {
        return site.fail(state, &error);
    }
    let image = cfg
        .run_and_update_ref_image
        .as_deref()
        .ok_or("run_async was called without a run-and-update-ref image")?;
    let task = async_work::prepare_task(subrequest, state.refname(), image)?;
    let record = state.conversation()?.async_task(&task)?;
    if record.is_none() {
        state.append(Transition::AsyncStart {
            record: AsyncRecord {
                task: task.clone(),
                status: AsyncStatus::Pending,
                target_ref: Some(state.refname().to_string()),
                result: None,
                reason: None,
            },
        })?;
    }
    let record = state
        .conversation()?
        .async_task(&task)?
        .ok_or_else(|| format!("async task {task} disappeared"))?;
    if record.target_ref.as_deref() != Some(state.refname()) {
        return Err(format!(
            "async task {task} targets {:?}, not {}",
            record.target_ref,
            state.refname()
        ));
    }
    if record.status == AsyncStatus::Pending {
        let dispatched = async_work::dispatch(&task);
        timing::phase("tool dispatch run_async");
        if let Err(error) = dispatched {
            eprintln!(
                "llm-step: {error}; the durable task state will cause a later recovery to retry"
            );
        }
    }
    let (status, result) = async_status(&record);
    let block = async_work::result_block(&call.id, &task, status, result.as_deref());
    site.complete(state, block, None, None, None)
}

fn async_request_error(request: &str) -> Result<Option<String>, String> {
    let base =
        std::env::var("CAOS_SERVER_URL").map_err(|_| "CAOS_SERVER_URL not set".to_string())?;
    let url = format!("{}/object/{request}", base.trim_end_matches('/'));
    let response = minreq::head(&url)
        .with_timeout(30)
        .send()
        .map_err(|error| format!("HEAD {url}: {error}"))?;
    match response.status_code {
        200..=299 => {}
        404 => {
            return Ok(Some(format!(
                "async request {request} is not stored in CAOS"
            )))
        }
        status => return Err(format!("HEAD {url}: {status} {}", response.reason_phrase)),
    }
    let target = fresh("async-subrequest");
    caos(["get-hash", request, &target])?;
    if !Path::new(&target).is_dir() {
        return Ok(Some(format!(
            "async request {request} is not an ArgTree (its object is not a tree)"
        )));
    }
    Ok(None)
}

fn async_status(record: &AsyncRecord) -> (&'static str, Option<String>) {
    match record.status {
        AsyncStatus::Pending => ("pending", None),
        AsyncStatus::Complete => ("complete", record.result.as_ref().map(ToString::to_string)),
        AsyncStatus::Failed => ("failed", record.result.as_ref().map(ToString::to_string)),
        AsyncStatus::Cancelled => ("cancelled", None),
    }
}

fn reconcile_async_tasks(state: &mut progress::State) -> Result<(), String> {
    let tasks = state.conversation()?.async_tasks()?;
    for task in tasks {
        if task.status != AsyncStatus::Pending
            || task.target_ref.as_deref() != Some(state.refname())
        {
            continue;
        }
        let (_, target) = async_work::task_request(&task.task)?;
        if target != state.refname() {
            return Err(format!(
                "async task {} targets {target}, not {}",
                task.task,
                state.refname()
            ));
        }
        let dispatched = async_work::dispatch(&task.task);
        timing::phase("tool dispatch run_async recovery");
        if let Err(error) = dispatched {
            eprintln!(
                "llm-step: could not re-admit async task {} (pending): {error}",
                task.task
            );
        }
    }
    Ok(())
}

fn reconcile_subagents(state: &mut progress::State) -> Result<(), String> {
    for child in state.conversation()?.children()? {
        if child.status != ChildStatus::Running {
            continue;
        }
        validate_child_relay(state, &child)?;
        let dispatched = subagents::dispatch(&child.relay);
        timing::phase("tool dispatch spawn_agent recovery");
        if let Err(error) = dispatched {
            eprintln!(
                "llm-step: could not re-admit subagent {} relay {} (running): {error}",
                child.id, child.relay
            );
        }
    }
    Ok(())
}

fn reconcile_background_tasks(state: &mut progress::State) -> Result<(), String> {
    reconcile_async_tasks(state)?;
    reconcile_subagents(state)
}

fn announce_background_tasks(
    conversation_id: &str,
    state: &mut progress::State,
) -> Result<(), String> {
    announce(conversation_id, state, pending_async_notices)?;
    announce(conversation_id, state, pending_subagent_notices)
}

fn announce(
    conversation_id: &str,
    state: &mut progress::State,
    pending: impl Fn(&str, &Conversation<'_>) -> Result<Vec<TranscriptEntry>, String>,
) -> Result<(), String> {
    loop {
        let view = state.conversation()?;
        let notice = pending(conversation_id, &view)?.into_iter().next();
        drop(view);
        let Some(entry) = notice else {
            return Ok(());
        };
        state.append(Transition::MessageAppend {
            entry,
            payloads: Vec::new(),
        })?;
    }
}

fn background_notice(conversation_id: &str, message_id: String, text: String) -> TranscriptEntry {
    TranscriptEntry {
        message_id,
        conversation: conversation_id.to_string(),
        role: Role::System,
        actor: "caos".to_string(),
        request: None,
        round: None,
        model: None,
        blocks: vec![Block::Text { text }],
        proposal: None,
        workspace_resolution: None,
    }
}

fn pending_async_notices(
    conversation_id: &str,
    view: &Conversation<'_>,
) -> Result<Vec<TranscriptEntry>, String> {
    let tasks = view
        .async_tasks()?
        .into_iter()
        .filter(|task| matches!(task.status, AsyncStatus::Complete | AsyncStatus::Failed))
        .collect::<Vec<_>>();
    if tasks.is_empty() {
        return Ok(Vec::new());
    }
    let existing: HashSet<String> = view
        .transcript(0, view.transcript_len()?)?
        .into_iter()
        .map(|(_, _, entry)| entry.message_id)
        .collect();
    let mut notices = Vec::new();
    for task in tasks {
        let message_id = ids::protocol_id("async-notice", &json!({"task":task.task.as_str()}))?;
        if existing.contains(&message_id) {
            continue;
        }
        let (status, result) = async_status(&task);
        let result = result.unwrap_or_else(|| "null".to_string());
        notices.push(background_notice(
            conversation_id,
            message_id,
            format!(
                "Independent task {} is {status}. Its result is {result}.",
                task.task
            ),
        ));
    }
    Ok(notices)
}

fn pending_subagent_notices(
    conversation_id: &str,
    view: &Conversation<'_>,
) -> Result<Vec<TranscriptEntry>, String> {
    let children = view
        .children()?
        .into_iter()
        .filter(|child| child.status != ChildStatus::Running)
        .collect::<Vec<_>>();
    if children.is_empty() {
        return Ok(Vec::new());
    }
    let existing: HashSet<String> = view
        .transcript(0, view.transcript_len()?)?
        .into_iter()
        .map(|(_, _, entry)| entry.message_id)
        .collect();
    let mut notices = Vec::new();
    for child in children {
        let terminal_head = child
            .terminal_head
            .as_ref()
            .ok_or_else(|| format!("terminal subagent {} has no terminal head", child.id))?;
        let message_id = ids::protocol_id(
            "subagent-notice",
            &json!({
                "child": child.id.as_str(),
                "terminal_head": terminal_head.as_str(),
            }),
        )?;
        if existing.contains(&message_id) {
            continue;
        }
        notices.push(background_notice(
            conversation_id,
            message_id,
            format!(
                "Subagent {} is {}. Its result is {terminal_head}.",
                child.id,
                child_status_text(child.status)
            ),
        ));
    }
    Ok(notices)
}

fn child_status_text(status: ChildStatus) -> &'static str {
    match status {
        ChildStatus::Running => "running",
        ChildStatus::Completed => "completed",
        ChildStatus::Failed => "failed",
        ChildStatus::Cancelled => "cancelled",
    }
}

fn drain(state: &mut progress::State, request: &Oid) -> Result<(), String> {
    loop {
        state.reload()?;
        let record = require_request(&state.conversation()?, request)?;
        if matches!(record.status, RequestStatus::Idle | RequestStatus::Failed) {
            return finish_from_terminal(state, request);
        }
        let round = round_state(&state.conversation()?, &record)?;
        let Some(call) = round.pending.first() else {
            let result = newest_assistant_path(&state.conversation()?, request)?;
            let terminal = state.append(Transition::RequestTerminal {
                request: request.clone(),
                outcome: RequestOutcome::Idle {
                    result,
                    interrupted: true,
                },
            })?;
            reconcile_background_tasks(state)?;
            return forward_result(state, &terminal.commit);
        };
        let reason = "interrupted before this tool ran";
        let block = error_block(&call.id, reason);
        close_pending_call(state, request, &round, call, &block, |_| {
            ToolResult::Cancelled {
                reason: reason.to_string(),
            }
        })?;
    }
}

fn newest_assistant_path(view: &Conversation<'_>, request: &Oid) -> Result<Option<String>, String> {
    last_matching(view, |ordinal, entry| {
        (entry.role == Role::Assistant && entry.request.as_ref() == Some(request))
            .then(|| paths::transcript_entry_path(ordinal, &entry.message_id))
    })
}

fn record_failure(
    cfg: &Config,
    state: &mut progress::State,
    request: &Oid,
    error: &str,
) -> Result<(), String> {
    loop {
        state.reload()?;
        let Some(record) = state.conversation()?.request(request)? else {
            return Ok(());
        };
        if matches!(record.status, RequestStatus::Idle | RequestStatus::Failed) {
            reconcile_background_tasks(state)?;
            return Ok(());
        }
        if !matches!(
            record.status,
            RequestStatus::Running | RequestStatus::Cancelling
        ) {
            return Ok(());
        }
        let round = round_state(&state.conversation()?, &record)?;
        if let Some(call) = round.pending.first() {
            let text = format!("the request stopped before this tool completed: {error}");
            let block = error_block(&call.id, &text);
            close_pending_call(state, request, &round, call, &block, |stub| {
                ToolResult::Failed {
                    error: observation_path(stub),
                }
            })?;
            continue;
        }
        let message_id = client_key()?;
        let appended = state.append(Transition::MessageAppend {
            entry: TranscriptEntry {
                message_id: message_id.clone(),
                conversation: cfg.conversation.clone(),
                role: Role::System,
                actor: "llm-step".to_string(),
                request: Some(request.clone()),
                round: Some(record.round),
                model: None,
                blocks: vec![Block::Text {
                    text: error.to_string(),
                }],
                proposal: None,
                workspace_resolution: None,
            },
            payloads: Vec::new(),
        })?;
        let ordinal = appended
            .ordinal
            .ok_or("failure message did not append a transcript entry")?;
        state.append(Transition::RequestTerminal {
            request: request.clone(),
            outcome: RequestOutcome::Failed {
                error: paths::transcript_entry_path(ordinal, &message_id),
            },
        })?;
        reconcile_background_tasks(state)?;
        return Ok(());
    }
}

fn finish_from_terminal(state: &mut progress::State, request: &Oid) -> Result<(), String> {
    let terminal = terminal_head(state, request)?;
    let outcome = require_request(&state.conversation_at(&terminal)?, request)?.outcome;
    let failure = match outcome {
        Some(RequestOutcome::Idle { .. }) => None,
        Some(RequestOutcome::Failed { error }) => {
            let view = state.conversation_at(&terminal)?;
            let (ordinal, message_id) = paths::parse_transcript_entry_path(&error)?;
            let (_, entry) = view
                .transcript_entry(ordinal)?
                .ok_or_else(|| format!("request error entry {error} does not exist"))?;
            if entry.message_id != message_id {
                return Err(format!(
                    "request error entry {error} has the wrong message id"
                ));
            }
            Some(
                entry
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        Block::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
                    .trim_end()
                    .to_string(),
            )
        }
        None => {
            return Err(format!(
                "request {request} has terminal status but no outcome"
            ))
        }
    };
    reconcile_background_tasks(state)?;
    match failure {
        Some(error) => Err(error),
        None => forward_result(state, &terminal),
    }
}

fn terminal_head(state: &progress::State, request: &Oid) -> Result<Oid, String> {
    terminal_head_in(state.store(), state.head(), request)
}

fn terminal_head_in(store: &dyn ObjectStore, head: &Oid, request: &Oid) -> Result<Oid, String> {
    let mut current = head.clone();
    let mut child = Conversation::open(store, &current)?.request(request)?;
    for _ in 0..MAX_SPINE_WALK {
        let info = store.read_commit(&current).map_err(String::from)?;
        let Some(parent) = info.parents.first() else {
            break;
        };
        let parent_record = Conversation::open(store, parent)?.request(request)?;
        let became_terminal = child.as_ref().is_some_and(|record| {
            matches!(record.status, RequestStatus::Idle | RequestStatus::Failed)
                && !parent_record.as_ref().is_some_and(|parent| {
                    matches!(parent.status, RequestStatus::Idle | RequestStatus::Failed)
                })
        });
        if became_terminal {
            return Ok(current);
        }
        current = parent.clone();
        child = parent_record;
    }
    Err(format!("request {request} has no request.terminal commit"))
}

fn forward_result(state: &mut progress::State, terminal: &Oid) -> Result<(), String> {
    let workspaces = state.conversation_at(terminal)?.workspaces()?;
    let dir = scratch("llm-step-result")?;
    let conversation_path = fresh("terminal-conversation");
    caos(["get-hash", terminal.as_str(), &conversation_path])?;
    link(&conversation_path, dir.join("conversation"))?;
    if !workspaces.is_empty() {
        let workspace_dir = dir.join("workspaces");
        fs::create_dir(&workspace_dir)
            .map_err(|error| format!("creating {}: {error}", workspace_dir.display()))?;
        for (name, workspace) in workspaces {
            let commit_path = fresh("terminal-workspace");
            caos(["get-hash", workspace.commit.as_str(), &commit_path])?;
            link(&commit_path, workspace_dir.join(name))?;
        }
    }
    caos(["put", path(&dir), "/cas/out"])
}

fn workspace_paths(state: &mut progress::State) -> Result<Vec<String>, String> {
    let workspaces = state.conversation()?.workspaces()?;
    let mut paths = Vec::with_capacity(workspaces.len());
    for workspace in workspaces.values() {
        paths.push(materialize_workspace(state, &workspace.commit)?.0);
    }
    Ok(paths)
}

fn registry(cfg: &Config, workspaces: &[String]) -> Result<Vec<Value>, String> {
    let mut registry = vec![with_workspace(bash_tool()), workspaces::declaration()];
    registry.extend(tools::declarations().into_iter().map(with_workspace));
    if cfg.run_and_update_ref_image.is_some() {
        registry.extend(subagents::declarations());
        registry.push(async_work::declaration());
    }
    if cfg.grep_image.is_some() {
        registry.push(with_workspace(tools::grep_declaration()));
    }
    if cfg.merge_image.is_some() {
        registry.push(with_workspace(merge_tool()));
    }
    if cfg.tools_image.is_some() {
        registry.extend(githist::declarations().into_iter().map(with_workspace));
    }
    for &(name, arg_name) in &STD_TOOLS {
        if cfg.std_tool_images.get(name).is_some_and(Option::is_some) {
            if let Some(tool) = tools::std_tool(name, &arg(arg_name))? {
                registry.push(with_workspace(tools::tree_tool_declaration(&tool)));
            }
        }
    }
    let mut dynamic_names = HashSet::new();
    for workspace in workspaces {
        for tool in tools::tree_tools(workspace)? {
            if dynamic_names.insert(tool.name.clone()) {
                registry.push(with_workspace(tools::tree_tool_declaration(&tool)));
            }
        }
    }
    Ok(registry)
}

fn with_workspace(mut declaration: Value) -> Value {
    if let Some(properties) = declaration
        .pointer_mut("/input_schema/properties")
        .and_then(Value::as_object_mut)
    {
        properties.insert(
            "workspace".to_string(),
            json!({
                "type":"string",
                "description":"Target workspace name. Required when there are several workspaces. Without workspace, inline file tools use files/ for conversation-owned files; an explicit workspace makes every path workspace-relative."
            }),
        );
    }
    declaration
}

fn std_tool_image<'a>(cfg: &'a Config, name: &str) -> Option<(&'a str, &'static str)> {
    let arg_name = STD_TOOLS
        .iter()
        .find_map(|&(tool, argument)| (tool == name).then_some(argument))?;
    cfg.std_tool_images
        .get(name)?
        .as_deref()
        .map(|image| (image, arg_name))
}

fn bash_tool() -> Value {
    json!({
        "name": "bash",
        "description": "Run a shell command in the workspace (executed with `sh -c` from the workspace root). Use this for COMMANDS (builds, tests, scripts); for plain file access prefer the read/ls/write/edit tools, which are immediate. The workspace is materialized lazily: ONLY the files and directories you list in `paths` are readable — a command touching any other existing path fails with 'Permission denied' (EACCES), and the result names the unmaterialized paths it touched. When that happens, retry the same command with those paths added to `paths`. Creating new files or directories needs no declaration. The result reports the exit code, stdout and stderr (tails), and the workspace carries all changes forward. A non-zero exit is reported back to you, not an error — read stderr and react.",
        "input_schema": {
            "type": "object",
            "properties": {
                "cmd": {"type":"string", "description":"The shell command to run."},
                "paths": {"type":"array", "items":{"type":"string"}, "description":"Workspace-relative paths the command reads or modifies; only these are materialized into the sandbox."}
            },
            "required": ["cmd"]
        }
    })
}

fn merge_tool() -> Value {
    json!({
        "name": "merge",
        "description": "Three-way merge another commit into the current workspace. `theirs` is a ref name from the snapshot (e.g. `main`, `origin/main`) or a commit hash; the current side is the workspace as it is now. A clean merge advances the workspace to the merged result. A conflict advances it too, with git's inline conflict markers in the files and a reserved `.caos/conflicts` file listing every unresolved path — including structural conflicts (delete/modify, mode, binary) that have NO markers. Resolve each: edit the file (use `read` with the stage's oid as `root` to inspect its content), then delete that path's rows from `.caos/conflicts`. Then build and test.",
        "input_schema": {
            "type":"object",
            "properties":{"theirs":{"type":"string","description":"The commit to merge in: a ref name from the snapshot, or a commit hash."}},
            "required":["theirs"]
        }
    })
}

fn resolve_theirs(cfg: &Config, call: &Value) -> Result<String, Value> {
    let id = call["id"].as_str().unwrap_or("");
    let theirs = call["input"]["theirs"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    lookup_theirs(cfg.merge_refs.as_deref(), theirs).map_err(|error| error_block(id, &error))
}

fn lookup_theirs(refs: Option<&str>, theirs: Option<&str>) -> Result<String, String> {
    let theirs = theirs
        .ok_or_else(|| "merge needs a string `theirs` (a ref name or a commit hash)".to_string())?;
    let mut names = Vec::new();
    for line in refs.unwrap_or("").lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((name, hash)) = line.split_once(char::is_whitespace) {
            let (name, hash) = (name.trim(), hash.trim());
            if name == theirs {
                return Ok(hash.to_string());
            }
            names.push(name.to_string());
        }
    }
    if (theirs.len() == 40 || theirs.len() == 64)
        && theirs
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Ok(theirs.to_string());
    }
    Err(format!(
        "unknown merge target {theirs:?}; available refs: {}",
        if names.is_empty() {
            "(none)".to_string()
        } else {
            names.join(", ")
        }
    ))
}

fn merge_result_block(id: &str, ws: &str) -> Result<Value, String> {
    let caos_dir = format!("{ws}/.caos");
    let mut conflicts = None;
    if Path::new(&caos_dir).exists() {
        caos(["get", &caos_dir])?;
        let file = format!("{caos_dir}/conflicts");
        if Path::new(&file).exists() {
            caos(["get", &file])?;
            conflicts = Some(
                fs::read_to_string(&file).map_err(|error| format!("reading conflicts: {error}"))?,
            );
        }
    }
    let text = match conflicts {
        Some(body) => format!(
            "merge produced conflicts. The workspace now carries git's inline conflict markers in the affected files, plus .caos/conflicts (git's unmerged notation, richer than markers). Resolve each path — edit the file, reading a stage's content with `read` (pass the stage oid as `root`) — then delete that path's rows from .caos/conflicts. Build and test when done.\n\n.caos/conflicts:\n{}",
            body.trim_end()
        ),
        None => "merge completed cleanly; the workspace is the merged result.".to_string(),
    };
    Ok(result_block(id, &text, false))
}

fn bash_result_block(id: &str) -> Result<Value, String> {
    caos(["get", &arg("result")])?;
    let leaf = |name: &str| -> Result<String, String> {
        let file = format!("{}/{name}", arg("result"));
        caos(["get", &file])?;
        let bytes = fs::read(&file).map_err(|error| format!("reading {file}: {error}"))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    let exit = leaf("exit")?.trim().to_string();
    let stdout = leaf("stdout")?;
    let stderr = leaf("stderr")?;
    let denied = if Path::new(&format!("{}/denied", arg("result"))).exists() {
        Some(leaf("denied")?)
    } else {
        None
    };
    let mut text = format!("exit: {exit}\nstdout:\n{stdout}\nstderr:\n{stderr}");
    if let Some(denied) = denied {
        text += &format!(
            "\nunmaterialized paths touched: {}; retry with them in `paths`.",
            denied.split_whitespace().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(result_block(id, &text, exit != "0"))
}

fn failed_run_block(id: &str, tool: &str, error: &str) -> Value {
    error_block(
        id,
        &format!(
            "the `{tool}` tool failed to run: {}\n\nThe workspace is unchanged. This is the tool itself failing, not a non-zero exit from your command.",
            error.trim_end()
        ),
    )
}

fn result_block(id: &str, text: &str, is_error: bool) -> Value {
    let mut block = json!({
        "type":"tool_result",
        "tool_use_id":id,
        "content":[{"type":"text","text":text}],
    });
    if is_error {
        block["is_error"] = Value::Bool(true);
    }
    block
}

fn error_block(id: &str, text: &str) -> Value {
    result_block(id, text, true)
}

fn literal_args(
    request: &str,
    oid: &Oid,
    what: &str,
    names: &[&str],
) -> Result<Vec<String>, String> {
    names
        .iter()
        .map(|name| {
            let argument = Path::new(request).join(name);
            if !argument.exists() {
                return Err(format!("{what} {oid} has no {name}"));
            }
            caos(["get", path(&argument)])?;
            if argument.is_dir() {
                return Err(format!("{what} {oid} has a tree-valued {name}"));
            }
            fs::read_to_string(&argument)
                .map(|value| value.trim().to_string())
                .map_err(|error| format!("reading {what} {oid} {name}: {error}"))
        })
        .collect()
}

fn literal_arg_tree(oid: &Oid, what: &str, names: &[&str]) -> Result<Vec<String>, String> {
    let materialized = fresh("literal-arg-tree");
    caos(["get-hash", oid.as_str(), &materialized])?;
    caos(["get", &materialized])?;
    if !Path::new(&materialized).is_dir() {
        return Err(format!("{what} {oid} is not an ArgTree"));
    }
    literal_args(&materialized, oid, what, names)
}

fn message(role: &str, content: Value) -> Value {
    json!({"role":role, "content":content})
}

fn canonical_payload_bytes(value: &Value) -> Result<Vec<u8>, String> {
    const PREFIX: &[u8] = b"{\"payload\":";
    let wrapped = canonical_bytes(&json!({"payload": value}))?;
    if !wrapped.starts_with(PREFIX) || !wrapped.ends_with(b"}\n") {
        return Err("canonical payload wrapper had an unexpected shape".to_string());
    }
    let mut bytes = wrapped[PREFIX.len()..wrapped.len() - 2].to_vec();
    bytes.push(b'\n');
    Ok(bytes)
}

fn user_text(text: &str) -> Value {
    message("user", Value::String(text.trim_end().to_string()))
}

fn response_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn validated_tool_calls(
    stop_reason: &str,
    tool_uses: &[Value],
) -> Result<Option<Vec<Value>>, String> {
    match stop_reason {
        "tool_use" => durable_tool_calls(tool_uses).map(Some),
        "end_turn" if !tool_uses.is_empty() => {
            Err("stop_reason end_turn but response contains tool_use blocks".to_string())
        }
        _ => Ok(None),
    }
}

fn durable_tool_calls(tool_uses: &[Value]) -> Result<Vec<Value>, String> {
    if tool_uses.is_empty() {
        return Err("stop_reason tool_use but no tool_use blocks".to_string());
    }
    let mut ids = HashSet::new();
    tool_uses
        .iter()
        .enumerate()
        .map(|(index, call)| {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("model tool_use block {index} has no string id"))?;
            if !ids.insert(id) {
                return Err(format!("model response repeats tool_use id {id:?}"));
            }
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("model tool_use block {index} has no string name"))?;
            Ok(json!({
                "id":id,
                "name":name,
                "args":call.get("input").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect()
}

fn client_key() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("minting transcript message id: {error}"))?;
    let key = conversation_protocol::v3::oid::hex_lower(&bytes);
    ids::validate_client_key(&key)?;
    Ok(key)
}

fn self_curry(
    wc: Option<&str>,
    request: &Oid,
    round: u64,
    current_id: &str,
    extras: &[(&str, Arg<'_>)],
) -> Result<String, String> {
    const MANAGED: &[&str] = &[
        "wc",
        "run",
        "round",
        "base-head",
        "current-id",
        "current-tool",
        "ws",
        "scope",
        "tool-eval",
        "tool-args",
        "tool-git",
        "in",
        "result",
        "error",
    ];
    let unbind: Vec<&str> = MANAGED
        .iter()
        .copied()
        .filter(|name| Path::new(&arg(name)).exists())
        .collect();
    let round = round.to_string();
    let mut bindings = Vec::new();
    if let Some(wc) = wc {
        bindings.push(("wc", Arg::Path(wc)));
    }
    bindings.extend([
        ("run", Arg::Lit(request.as_str())),
        ("round", Arg::Lit(round.as_str())),
        ("current-id", Arg::Lit(current_id)),
    ]);
    bindings.extend_from_slice(extras);
    caos_recurry(Arg::Hash(&own_args_tree()?), &unbind, &bindings)
}

pub(crate) fn fresh_name(prefix: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{counter}")
}

pub(crate) fn fresh(prefix: &str) -> String {
    format!("/cas/{}", fresh_name(prefix))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use conversation_protocol::v3::apply::{apply as apply_transition, client_signature, mint};
    use conversation_protocol::v3::oid::ensure_genesis;
    use conversation_protocol::v3::{
        GitStore, Identity, IdentityKind, MemoryStore, RefUpdate, TreeBuilder, WorkspaceOrigin,
    };

    use super::*;

    const CONVERSATION: &str = "conversation";
    const USER_ID: &str = "11111111111111111111111111111111";
    const ASSISTANT_ID: &str = "22222222222222222222222222222222";

    struct Golden {
        store: MemoryStore,
        head: Oid,
        request: Oid,
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(fresh_name(label));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        fn child(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove test directory");
        }
    }

    fn init_git(path: &Path, bare: bool) {
        let mut command = Command::new("git");
        command.args(["init", "--quiet", "--object-format=sha1"]);
        if bare {
            command.arg("--bare");
        }
        assert!(command.arg(path).status().expect("run git init").success());
    }

    fn add_git_remote(repository: &Path, remote: &Path) {
        let remote = format!("file://{}", remote.display());
        assert!(Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["remote", "add", "caos", &remote])
            .status()
            .expect("add git remote")
            .success());
    }

    fn append_memory(
        store: &mut MemoryStore,
        parent: &Oid,
        transition: Transition,
    ) -> Result<Oid, String> {
        let parent_tree = store.read_commit(parent).map_err(String::from)?.tree;
        let applied = apply_transition(store, Some(&parent_tree), &transition)?;
        let signature = inherited_signature(store, parent)?;
        mint(store, parent, &applied.tree, transition.kind(), &signature)
    }

    fn root_with(
        store: &mut dyn ObjectStore,
        workspaces: BTreeMap<String, (Oid, Option<WorkspaceOrigin>)>,
    ) -> Result<Oid, String> {
        let genesis = ensure_genesis(store)?;
        let transition = Transition::ConversationRoot {
            identity: Identity {
                id: CONVERSATION.to_string(),
                kind: IdentityKind::Root,
                owner: None,
            },
            title: "test conversation".to_string(),
            workspaces,
            files_seed: None,
        };
        let applied = apply_transition(store, None, &transition)?;
        mint(
            store,
            &genesis,
            &applied.tree,
            transition.kind(),
            &client_signature("test", "test@example.invalid", 1),
        )
    }

    #[test]
    fn reconciles_remote_only_proposal_tree_as_merged() {
        let directory = TestDirectory::new("llm-step-remote-tree");
        let remote = directory.child("remote.git");
        let writer = directory.child("writer");
        let client = directory.child("client");
        init_git(&remote, true);
        init_git(&writer, false);
        init_git(&client, false);
        add_git_remote(&writer, &remote);
        add_git_remote(&client, &remote);
        assert!(Command::new("git")
            .arg("-C")
            .arg(&remote)
            .args(["config", "uploadpack.allowAnySHA1InWant", "true"])
            .status()
            .expect("configure object fetch")
            .success());

        let mut writer_store = GitStore::open(&writer, Some("caos")).unwrap();
        let base_tree = writer_store.write_tree(&[]).unwrap();
        let signature = client_signature("test", "test@example.invalid", 1);
        let base = writer_store
            .commit(&base_tree, &[], "base\n", &signature)
            .unwrap();
        let mut current_builder = TreeBuilder::from(Some(base_tree.clone()));
        current_builder.put("current", Mode::Blob, b"current\n".to_vec());
        let current_tree = current_builder.build(&mut writer_store).unwrap();
        let current = writer_store
            .commit(
                &current_tree,
                std::slice::from_ref(&base),
                "current\n",
                &signature,
            )
            .unwrap();
        let mut proposal_builder = TreeBuilder::from(Some(base_tree));
        proposal_builder.put("proposal", Mode::Blob, b"proposal\n".to_vec());
        let proposal_tree = proposal_builder.build(&mut writer_store).unwrap();
        let root = root_with(&mut writer_store, BTreeMap::new()).unwrap();
        let conversation_ref = conversation_protocol::v3::refs::head_ref(CONVERSATION).unwrap();
        writer_store
            .push(&[
                RefUpdate {
                    refname: conversation_ref.clone(),
                    expected: None,
                    new: Some(root.clone()),
                },
                RefUpdate {
                    refname: format!("refs/caos/req/{current}"),
                    expected: None,
                    new: Some(current.clone()),
                },
                RefUpdate {
                    refname: format!("refs/caos/req/{proposal_tree}"),
                    expected: None,
                    new: Some(proposal_tree.clone()),
                },
            ])
            .unwrap();

        let client_store = GitStore::open(&client, Some("caos")).unwrap();
        assert_eq!(
            client_store.fetch_ref(&conversation_ref).unwrap(),
            Some(root.clone())
        );
        client_store.fetch_object(&current).unwrap();
        assert!(!client_store.has_local(&proposal_tree).unwrap());
        let mut state = progress::State::from_store(client_store, conversation_ref, root).unwrap();
        let proposal = mint_workspace_commit(&mut state, &proposal_tree, &base, "bash").unwrap();
        assert!(state.store().has_local(&proposal_tree).unwrap());
        let resolution = reconcile(
            state.store_mut(),
            &base,
            &proposal,
            Some(&current),
            &signature,
        )
        .unwrap();
        assert!(matches!(resolution, WorkspaceResolution::Merged { .. }));
    }

    fn test_entry(message_id: &str, role: Role, blocks: Vec<Block>) -> TranscriptEntry {
        TranscriptEntry {
            message_id: message_id.to_string(),
            conversation: CONVERSATION.to_string(),
            role,
            actor: "test".to_string(),
            request: None,
            round: None,
            model: None,
            blocks,
            proposal: None,
            workspace_resolution: None,
        }
    }

    fn golden_two_calls() -> Result<Golden, String> {
        let mut store = MemoryStore::new();
        let root = root_with(&mut store, BTreeMap::new())?;
        let user = append_memory(
            &mut store,
            &root,
            Transition::MessageAppend {
                entry: test_entry(
                    USER_ID,
                    Role::User,
                    vec![Block::Text {
                        text: "  inspect this  ".to_string(),
                    }],
                ),
                payloads: Vec::new(),
            },
        )?;
        let request = Oid::parse(&"a".repeat(40), "request")?;
        let request_workspaces = Conversation::open(&store, &user)?.workspaces_tree()?;
        let admitted = append_memory(
            &mut store,
            &user,
            Transition::RequestAdmit {
                record: RequestRecord {
                    id: request.clone(),
                    request_head: user.clone(),
                    request_workspaces,
                    model: "test-model".to_string(),
                    configuration: "test-config".to_string(),
                    round: 0,
                    calls: Vec::new(),
                    interjections: Vec::new(),
                    status: RequestStatus::Queued,
                    latest_message: None,
                    escape_reason: None,
                    outcome: None,
                },
            },
        )?;
        let claimed = append_memory(
            &mut store,
            &admitted,
            Transition::RequestClaim {
                request: request.clone(),
                latest_message: USER_ID.to_string(),
            },
        )?;
        let first_args = format!(
            "{}/args-first.json",
            paths::transcript_payload_dir(1, ASSISTANT_ID)
        );
        let second_args = format!(
            "{}/args-second.json",
            paths::transcript_payload_dir(1, ASSISTANT_ID)
        );
        let response = vec![
            json!({"type":"text", "text":"I will inspect it."}),
            json!({"type":"tool_use", "id":"first", "name":"read", "input":{"file-path":"files/a"}}),
            json!({"type":"tool_use", "id":"second", "name":"ls", "input":{"path":"files"}}),
        ];
        let head = append_memory(
            &mut store,
            &claimed,
            Transition::ModelComplete {
                request: request.clone(),
                entry: TranscriptEntry {
                    message_id: ASSISTANT_ID.to_string(),
                    conversation: CONVERSATION.to_string(),
                    role: Role::Assistant,
                    actor: "test-model".to_string(),
                    request: Some(request.clone()),
                    round: Some(0),
                    model: Some("test-model".to_string()),
                    blocks: vec![
                        Block::Text {
                            text: "I will inspect it.".to_string(),
                        },
                        Block::ToolUse {
                            id: "first".to_string(),
                            name: "read".to_string(),
                            arguments: first_args,
                        },
                        Block::ToolUse {
                            id: "second".to_string(),
                            name: "ls".to_string(),
                            arguments: second_args,
                        },
                    ],
                    proposal: None,
                    workspace_resolution: None,
                },
                payloads: vec![
                    (
                        "response.json".to_string(),
                        canonical_payload_bytes(&Value::Array(response))?,
                    ),
                    (
                        "args-first.json".to_string(),
                        canonical_bytes(&json!({"file-path":"files/a"}))?,
                    ),
                    (
                        "args-second.json".to_string(),
                        canonical_bytes(&json!({"path":"files"}))?,
                    ),
                ],
                calls: vec![
                    DeclaredCall {
                        id: "first".to_string(),
                        name: "read".to_string(),
                    },
                    DeclaredCall {
                        id: "second".to_string(),
                        name: "ls".to_string(),
                    },
                ],
            },
        )?;
        Ok(Golden {
            store,
            head,
            request,
        })
    }

    fn terminal_tool(
        request: &Oid,
        id: &str,
        name: &str,
        result: ToolResult,
        block: Value,
    ) -> Result<Transition, String> {
        let call = Call {
            id: id.to_string(),
            name: name.to_string(),
            input: Value::Null,
        };
        let stub = CallSite::at(request, 0, &call, ASSISTANT_ID).stub(None);
        let record = completed_record(&stub, result, None);
        tool_complete_transition(record, &block, Vec::new())
    }

    #[test]
    fn durable_tool_calls_reject_unreplayable_ids() {
        let valid = json!({"type":"tool_use","id":"same","name":"read","input":{}});
        assert_eq!(
            durable_tool_calls(std::slice::from_ref(&valid)).unwrap()[0]["id"],
            "same"
        );
        let missing = json!({"type":"tool_use","name":"read","input":{}});
        assert!(durable_tool_calls(&[missing])
            .unwrap_err()
            .contains("no string id"));
        assert!(durable_tool_calls(&[valid.clone(), valid])
            .unwrap_err()
            .contains("repeats tool_use id"));
    }

    #[test]
    fn terminal_response_rejects_tool_calls_before_recording() {
        let call = json!({"type":"tool_use","id":"call","name":"read","input":{}});
        assert!(validated_tool_calls("end_turn", &[]).unwrap().is_none());
        assert!(
            validated_tool_calls("end_turn", std::slice::from_ref(&call))
                .unwrap_err()
                .contains("end_turn")
        );
        assert_eq!(
            validated_tool_calls("tool_use", &[call]).unwrap().unwrap()[0]["id"],
            "call"
        );
    }

    #[test]
    fn max_token_continuation_prefills_the_partial_assistant_blocks() {
        let mut messages = vec![user_text("prompt")];
        let blocks = vec![json!({"type":"text","text":"partial"})];
        append_max_tokens_prefill(&mut messages, blocks.clone());
        assert_eq!(messages[1], json!({"role":"assistant","content":blocks}));
    }

    #[test]
    fn context_rebuilds_a_complete_tool_batch_in_declaration_order() {
        let mut golden = golden_two_calls().unwrap();
        let first_block = json!({
            "type":"tool_result", "tool_use_id":"first", "content":"one"
        });
        let first = terminal_tool(
            &golden.request,
            "first",
            "read",
            ToolResult::Complete {
                observation: format!(
                    "{}/observation.json",
                    paths::tool_payload_dir(golden.request.as_str(), 0, "first")
                ),
                proposal: None,
            },
            first_block.clone(),
        )
        .unwrap();
        golden.head = append_memory(&mut golden.store, &golden.head, first).unwrap();
        let view = Conversation::open(&golden.store, &golden.head).unwrap();
        let request = require_request(&view, &golden.request).unwrap();
        assert_eq!(round_state(&view, &request).unwrap().pending.len(), 1);
        assert_eq!(context_messages(&view).unwrap().len(), 2);

        let second_block = json!({
            "type":"tool_result", "tool_use_id":"second", "content":"two"
        });
        let second = terminal_tool(
            &golden.request,
            "second",
            "ls",
            ToolResult::Complete {
                observation: format!(
                    "{}/observation.json",
                    paths::tool_payload_dir(golden.request.as_str(), 0, "second")
                ),
                proposal: None,
            },
            second_block.clone(),
        )
        .unwrap();
        golden.head = append_memory(&mut golden.store, &golden.head, second).unwrap();
        golden.head = append_memory(
            &mut golden.store,
            &golden.head,
            Transition::MessageAppend {
                entry: test_entry(
                    "33333333333333333333333333333333",
                    Role::System,
                    vec![Block::Text {
                        text: "Independent task finished.".to_string(),
                    }],
                ),
                payloads: Vec::new(),
            },
        )
        .unwrap();
        let view = Conversation::open(&golden.store, &golden.head).unwrap();
        assert!(
            round_state(&view, &require_request(&view, &golden.request).unwrap())
                .unwrap()
                .pending
                .is_empty()
        );
        let messages = context_messages(&view).unwrap();
        assert_eq!(
            messages[0],
            json!({"role":"user", "content":"  inspect this"})
        );
        assert_eq!(
            messages[2],
            message("user", json!([first_block, second_block]))
        );
        assert_eq!(messages[3], user_text("Independent task finished."));
    }

    #[test]
    fn drain_closes_every_pending_call() {
        let mut golden = golden_two_calls().unwrap();
        loop {
            let view = Conversation::open(&golden.store, &golden.head).unwrap();
            let request = require_request(&view, &golden.request).unwrap();
            let round = round_state(&view, &request).unwrap();
            let Some(call) = round.pending.first() else {
                break;
            };
            let reason = "interrupted before this tool ran";
            let transition = terminal_tool(
                &golden.request,
                &call.id,
                &call.name,
                ToolResult::Cancelled {
                    reason: reason.to_string(),
                },
                error_block(&call.id, reason),
            )
            .unwrap();
            golden.head = append_memory(&mut golden.store, &golden.head, transition).unwrap();
        }
        let view = Conversation::open(&golden.store, &golden.head).unwrap();
        for id in ["first", "second"] {
            assert_eq!(
                view.tool(&golden.request, 0, id).unwrap().unwrap().status,
                ToolStatus::Cancelled
            );
        }
    }

    #[test]
    fn async_notice_is_idempotent() {
        let mut store = MemoryStore::new();
        let root = root_with(&mut store, BTreeMap::new()).unwrap();
        let task = Oid::parse(&"b".repeat(40), "task").unwrap();
        let result = Oid::parse(&"c".repeat(40), "result").unwrap();
        let pending = append_memory(
            &mut store,
            &root,
            Transition::AsyncStart {
                record: AsyncRecord {
                    task: task.clone(),
                    status: AsyncStatus::Pending,
                    target_ref: Some("refs/caos/v3/conversations/conversation/head".to_string()),
                    result: None,
                    reason: None,
                },
            },
        )
        .unwrap();
        let terminal = append_memory(
            &mut store,
            &pending,
            Transition::AsyncTerminal {
                task: task.clone(),
                status: AsyncStatus::Failed,
                result: Some(result),
                reason: None,
            },
        )
        .unwrap();
        let view = Conversation::open(&store, &terminal).unwrap();
        let notices = pending_async_notices(CONVERSATION, &view).unwrap();
        assert_eq!(notices.len(), 1);
        let notice_id = ids::protocol_id("async-notice", &json!({"task":task.as_str()})).unwrap();
        assert_eq!(notices[0].message_id, notice_id);
        assert_eq!(
            notices[0].blocks,
            vec![Block::Text {
                text: format!(
                    "Independent task {task} is failed. Its result is {}.",
                    "c".repeat(40)
                )
            }]
        );
        let announced = append_memory(
            &mut store,
            &terminal,
            Transition::MessageAppend {
                entry: notices[0].clone(),
                payloads: Vec::new(),
            },
        )
        .unwrap();
        assert!(pending_async_notices(
            CONVERSATION,
            &Conversation::open(&store, &announced).unwrap()
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn subagent_notice_is_idempotent_and_names_the_checkpoint() {
        let mut store = MemoryStore::new();
        let head = conversation_protocol::v3::fixtures::golden(&mut store);
        let view = Conversation::open(&store, &head).unwrap();
        let child = view.children().unwrap().into_iter().next().unwrap();
        let terminal = child.terminal_head.clone().unwrap();
        let notices = pending_subagent_notices("golden-conversation", &view).unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].blocks,
            vec![Block::Text {
                text: format!(
                    "Subagent {} is completed. Its result is {terminal}.",
                    child.id
                )
            }]
        );
        let announced = append_memory(
            &mut store,
            &head,
            Transition::MessageAppend {
                entry: notices[0].clone(),
                payloads: Vec::new(),
            },
        )
        .unwrap();
        assert!(pending_subagent_notices(
            "golden-conversation",
            &Conversation::open(&store, &announced).unwrap()
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn queued_escape_is_the_request_terminal_head() {
        let mut store = MemoryStore::new();
        let root = root_with(&mut store, BTreeMap::new()).unwrap();
        let user = append_memory(
            &mut store,
            &root,
            Transition::MessageAppend {
                entry: test_entry(
                    USER_ID,
                    Role::User,
                    vec![Block::Text {
                        text: "stop".to_string(),
                    }],
                ),
                payloads: Vec::new(),
            },
        )
        .unwrap();
        let request = test_oid('d');
        let request_workspaces = Conversation::open(&store, &user)
            .unwrap()
            .workspaces_tree()
            .unwrap();
        let admitted = append_memory(
            &mut store,
            &user,
            Transition::RequestAdmit {
                record: RequestRecord {
                    id: request.clone(),
                    request_head: user.clone(),
                    request_workspaces,
                    model: "test-model".to_string(),
                    configuration: "test-config".to_string(),
                    round: 0,
                    calls: Vec::new(),
                    interjections: Vec::new(),
                    status: RequestStatus::Queued,
                    latest_message: None,
                    escape_reason: None,
                    outcome: None,
                },
            },
        )
        .unwrap();
        let escaped = append_memory(
            &mut store,
            &admitted,
            Transition::RequestEscape {
                request: request.clone(),
                reason: Some("escape".to_string()),
            },
        )
        .unwrap();
        assert_eq!(
            terminal_head_in(&store, &escaped, &request).unwrap(),
            escaped
        );
    }

    #[test]
    fn explicit_workspace_files_path_targets_workspace() {
        let mut store = MemoryStore::default();
        let head = root_with(
            &mut store,
            BTreeMap::from([("main".into(), (test_oid('a'), None))]),
        )
        .unwrap();
        let view = Conversation::open(&store, &head).unwrap();
        for (name, path_arg) in [
            ("read", "file-path"),
            ("write", "file-path"),
            ("edit", "file-path"),
            ("ls", "path"),
        ] {
            let mut call = Call {
                id: "call-1".into(),
                name: name.into(),
                input: json!({"workspace":"main", path_arg:"files/config.json"}),
            };
            assert!(
                matches!(
                    resolve_target(&view, &call).unwrap(),
                    Target::Workspace { .. }
                ),
                "{name}"
            );
            call.input["workspace"] = json!("missing");
            assert!(resolve_target(&view, &call)
                .unwrap_err()
                .contains("unknown workspace"));
            call.input.as_object_mut().unwrap().remove("workspace");
            assert!(
                matches!(resolve_target(&view, &call).unwrap(), Target::Files),
                "{name}"
            );
        }
    }

    #[test]
    fn target_resolution_errors_name_available_workspaces() {
        let mut store = MemoryStore::new();
        let empty = root_with(&mut store, BTreeMap::new()).unwrap();
        let call = Call {
            id: "call".to_string(),
            name: "bash".to_string(),
            input: json!({"cmd":"true"}),
        };
        assert_eq!(
            resolve_target(&Conversation::open(&store, &empty).unwrap(), &call).unwrap_err(),
            "this conversation has no workspace"
        );
        let workspaces = BTreeMap::from([
            ("api".to_string(), (test_oid('a'), None)),
            ("web".to_string(), (test_oid('b'), None)),
        ]);
        let two = root_with(&mut store, workspaces).unwrap();
        assert_eq!(
            resolve_target(&Conversation::open(&store, &two).unwrap(), &call).unwrap_err(),
            "several workspaces: api, web; pass workspace=<name>"
        );
    }

    fn test_oid(character: char) -> Oid {
        Oid::parse(&character.to_string().repeat(40), "test oid").unwrap()
    }

    #[test]
    fn theirs_lookup() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let refs = format!("main {a}\norigin/main {b}\n");
        assert_eq!(lookup_theirs(Some(&refs), Some("main")).unwrap(), a);
        assert_eq!(lookup_theirs(Some(&refs), Some("origin/main")).unwrap(), b);
        let error = lookup_theirs(Some(&refs), Some("missing")).unwrap_err();
        assert!(error.contains("main") && error.contains("origin/main"));
    }
}
