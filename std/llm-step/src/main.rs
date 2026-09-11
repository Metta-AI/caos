//! The agent turn driver for the v3 conversation protocol.

mod async_work;
mod githist;
mod progress;
mod source_trees;
mod subagents;
mod timing;
mod tools;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use conversation_protocol::v3::apply::{apply, inherited_signature, mint, Transition};
use conversation_protocol::v3::canonical::canonical_bytes;
use conversation_protocol::v3::ids;
use conversation_protocol::v3::paths;
use conversation_protocol::v3::view::Conversation;
use conversation_protocol::v3::{
    reconcile, validate_spine, AsyncRecord, Block, CallRecord, CallStatus, ChildRecord, CodeOps,
    DeclaredCall, FilesOutcome, Identity, IdentityKind, Mode, ObjectStore, Oid, Owner, Role,
    SourceTreeResolution, SpawnIntent, TaskStatus, ToolResult, TranscriptEntry, TurnOutcome,
    TurnRecord, TurnStatus,
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

#[derive(Clone)]
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
            TurnStatus::Queued => {
                let latest_message = newest_user_message(&state.conversation()?)?;
                let expected = state.head().clone();
                match state.try_append_at(
                    &expected,
                    Transition::TurnClaim {
                        request: request.clone(),
                        latest_message,
                    },
                )? {
                    progress::TryAppend::Appended(_) => break,
                    progress::TryAppend::HeadChanged(_) => continue,
                }
            }
            TurnStatus::Running => break,
            TurnStatus::Cancelling => return drain(state, request),
            TurnStatus::Idle | TurnStatus::Failed => return finish_from_terminal(state, request),
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

fn require_request(view: &Conversation<'_>, request: &Oid) -> Result<TurnRecord, String> {
    view.turn(request)?
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
    if matches!(status, TurnStatus::Idle | TurnStatus::Failed) {
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
        return launch_evaluated_tool(cfg, state, request, request_head, round, &id);
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
        Target::SourceTree { name, commit } => Some((name, commit)),
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
            TurnStatus::Cancelling => return drain(state, request),
            TurnStatus::Idle | TurnStatus::Failed => return finish_from_terminal(state, request),
            TurnStatus::Queued => return start(cfg, state, request, request_head),
            TurnStatus::Running => {}
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
        if current.status != TurnStatus::Running {
            continue;
        }
        let round = round_state(&state.conversation()?, &current)?;
        if !round.pending.is_empty() {
            continue;
        }
        let messages = context_messages(&state.conversation()?)?;
        let source_trees = source_tree_paths(state)?;
        let previous = state.head().clone();
        return llm_round(
            cfg,
            state,
            request,
            request_head,
            messages,
            &source_trees,
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

    fn stub(&self, target: Option<(String, Oid)>) -> CallRecord {
        let (source_tree_name, input_commit) = match target {
            Some((name, commit)) => (Some(name), Some(commit)),
            None => (None, None),
        };
        CallRecord {
            request: self.request.clone(),
            round: self.round,
            id: self.call.id.clone(),
            name: self.call.name.clone(),
            declaration_message: self.declaration.to_string(),
            source_tree_name,
            input_commit,
            status: CallStatus::Complete,
            task: None,
            result: None,
            source_tree_resolution: None,
            files: Vec::new(),
            files_outcome: None,
        }
    }

    fn finish(
        &self,
        state: &mut progress::State,
        block: &Value,
        target: Option<(String, Oid)>,
        resolution: Option<SourceTreeResolution>,
        task: Option<Oid>,
        result: impl FnOnce(&CallRecord) -> ToolResult,
    ) -> Result<(), String> {
        let mut stub = self.stub(target);
        stub.task = task;
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
        resolution: Option<SourceTreeResolution>,
    ) -> Result<(), String> {
        self.finish(state, &block, target, resolution, None, |stub| {
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
        self.finish(state, block, target, None, None, |stub| {
            ToolResult::Failed {
                error: observation_path(stub),
            }
        })
    }
}

fn close_pending_call(
    state: &mut progress::State,
    request: &Oid,
    round: &RoundState,
    call: &Call,
    block: &Value,
    result: impl FnOnce(&CallRecord) -> ToolResult,
) -> Result<(), String> {
    let existing = state
        .conversation()?
        .tool(request, round.declaring_round, &call.id)?;
    let target = existing
        .as_ref()
        .and_then(|record| {
            record
                .source_tree_name
                .clone()
                .zip(record.input_commit.clone())
        })
        .or_else(
            || match resolve_target(&state.conversation().ok()?, call).ok()? {
                Target::SourceTree { name, commit } => Some((name, commit)),
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

fn round_state(view: &Conversation<'_>, record: &TurnRecord) -> Result<RoundState, String> {
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
                        paths::call_payload_dir(request.as_str(), round, id)
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
    tool: &CallRecord,
    observation: Value,
) -> Result<Value, String> {
    if observation.get("type").and_then(Value::as_str) == Some("tool_result") {
        return Ok(observation);
    }
    if tool.name == subagents::SPAWN_TOOL {
        let child_id = observation["child"]
            .as_str()
            .ok_or("spawn observation has no child")?;
        let child = view
            .child(child_id)?
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
        tool.status == CallStatus::Conflict,
    ))
}

#[allow(clippy::too_many_arguments)]
fn llm_round(
    cfg: &Config,
    state: &mut progress::State,
    request: &Oid,
    request_head: &Oid,
    messages: Vec<Value>,
    source_trees: &[String],
    previous: &Oid,
    round: u64,
) -> Result<(), String> {
    let body = json!({
        "model": cfg.model,
        "max_tokens": MAX_TOKENS,
        "thinking": {"type": "adaptive"},
        "cache_control": {"type": "ephemeral"},
        "system": format!("{}{}", cfg.system, format!("{}{}", source_trees::context(&state.conversation()?)?, source_trees::repository_context(&state.conversation()?, source_trees)?)),
        "tools": registry(cfg)?,
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
        TurnStatus::Cancelling => return drain(state, request),
        TurnStatus::Idle | TurnStatus::Failed => return finish_from_terminal(state, request),
        TurnStatus::Queued => {
            return Err(format!(
                "request {request} returned to queued after model completion"
            ))
        }
        TurnStatus::Running => {}
    }
    let terminal = Transition::TurnTerminal {
        request: request.clone(),
        outcome: TurnOutcome::Idle {
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
            forward_result(&terminal.commit)
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
        source_tree_resolution: None,
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
    if record.status == TurnStatus::Cancelling && record.round == round {
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
    SourceTree { name: String, commit: Oid },
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
        if existing.status == CallStatus::Started {
            if existing.name == subagents::WAIT_TOOL {
                dispatch_wait_started(state, request, round.declaring_round, call, &existing)?;
            } else {
                dispatch_started(state, request, round.declaring_round, call, &existing)?;
            }
            return Ok(false);
        }
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
    let (name, commit, scoped) = match target {
        Target::SourceTree { name, commit } => {
            let scoped = cfg.clone();
            (Some(name), commit, scoped)
        }
        Target::Files => (None, state.head().clone(), cfg.clone()),
    };
    let (ws, wc) = materialize_source_tree(state, &commit)?;
    let mut scoped_call = call.clone();
    if call.name == "run_tool" {
        if let Some(name) = &name {
            let path = call.input["path"]
                .as_str()
                .ok_or("run_tool requires path")?;
            scoped_call.input["path"] = json!(path
                .strip_prefix(&format!("{name}/"))
                .ok_or("tool path is outside its source tree")?);
        }
    }
    match prepare_compute(&scoped, &scoped_call, &ws, &wc, request, round)? {
        Prepared::Result(block) => {
            site.complete(state, block, name.map(|name| (name, commit)), None, None)?;
            Ok(true)
        }
        Prepared::Evaluation => Ok(false),
        Prepared::Task(task) => {
            let record = CallRecord {
                request: request.clone(),
                round: round.declaring_round,
                id: call.id.clone(),
                name: call.name.clone(),
                declaration_message: round.declaration_message.clone(),
                source_tree_name: name,
                input_commit: Some(commit),
                status: CallStatus::Started,
                task: Some(task.clone()),
                result: None,
                source_tree_resolution: None,
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
        .get("source_tree")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if requested.is_none() {
        if matches!(call.name.as_str(), "bash" | "grep") {
            return Ok(Target::Files);
        }
        if tools::is_inline(&call.name) {
            return Ok(Target::Files);
        }
        if call.name == "run_tool" {
            let path = call.input["path"]
                .as_str()
                .ok_or("run_tool requires path")?;
            paths::validate_tree_path(path)?;
            for name in view.source_tree_names()?.into_iter().rev() {
                if path.starts_with(&format!("{name}/")) {
                    let commit = view
                        .source_tree(&name)?
                        .ok_or("reference disappeared")?
                        .commit;
                    return Ok(Target::SourceTree { name, commit });
                }
            }
            return Ok(Target::Files);
        }
    }
    let (name, commit) = source_tree_target(view, requested)?
        .ok_or_else(|| "this conversation has no source tree".to_string())?;
    Ok(Target::SourceTree { name, commit })
}

fn source_tree_target(
    view: &Conversation<'_>,
    requested: Option<&str>,
) -> Result<Option<(String, Oid)>, String> {
    let source_trees = view.source_trees()?;
    let name = requested
        .ok_or("specify the source_tree path for this Git operation")?
        .to_string();
    let Some(source_tree) = source_trees.get(&name) else {
        return Err(format!(
            "unknown source tree {name:?}; available source trees: {}",
            if source_trees.is_empty() {
                "(none)".to_string()
            } else {
                source_trees.keys().cloned().collect::<Vec<_>>().join(", ")
            }
        ));
    };
    Ok(Some((name, source_tree.commit.clone())))
}

fn inline_files_path(call: &Call) -> Option<(&'static str, String)> {
    if !tools::is_inline(&call.name) {
        return None;
    }
    let key = if call.name == "ls" {
        "path"
    } else {
        "file-path"
    };
    let path = call
        .input
        .get(key)
        .and_then(Value::as_str)
        .or_else(|| (call.name == "ls").then_some("."))?
        .trim()
        .trim_start_matches('/');
    Some((key, path.to_string()))
}

fn materialize_source_tree(
    state: &mut progress::State,
    commit: &Oid,
) -> Result<(String, String), String> {
    static MATERIALIZED: OnceLock<Mutex<HashMap<Oid, (String, String)>>> = OnceLock::new();
    let mut memo = MATERIALIZED
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "source tree materialization cache is poisoned".to_string())?;
    if let Some(paths) = memo.get(commit).cloned() {
        return Ok(paths);
    }
    state.fetch_object(commit)?;
    let tree = state.store().tree_of(commit)?;
    let ws = fresh("source_tree");
    caos(["get-hash", tree.as_str(), &ws])?;
    let wc = fresh("source-tree-commit");
    caos(["get-hash", commit.as_str(), &wc])?;
    memo.insert(commit.clone(), (ws.clone(), wc.clone()));
    Ok((ws, wc))
}

enum Prepared {
    Result(Value),
    Evaluation,
    Task(Oid),
}

#[allow(clippy::too_many_arguments)]
fn prepare_compute(
    cfg: &Config,
    call: &Call,
    ws: &str,
    wc: &str,
    request: &Oid,
    round: &RoundState,
) -> Result<Prepared, String> {
    let clean = call_without_source_tree(call);
    match call.name.as_str() {
        "bash" => prepare_bash(cfg, &clean, ws),
        "run_tool" => {
            let Some(relative) = clean["input"]["path"].as_str() else {
                return Ok(Prepared::Result(error_block(
                    &call.id,
                    "run_tool requires path",
                )));
            };
            let tool = match tools::tool_at(ws, relative) {
                Ok(Some(tool)) => tool,
                Ok(None) => {
                    return Ok(Prepared::Result(error_block(
                        &call.id,
                        "path is not a tool",
                    )))
                }
                Err(error) => return Ok(Prepared::Result(error_block(&call.id, &error))),
            };
            let nested = json!({"id":call.id, "input":clean["input"].get("arguments")
                .cloned().unwrap_or_else(|| json!({}))});
            match tools::tree_tool_args(&nested, &tool) {
                Err(block) => Ok(Prepared::Result(block)),
                Ok(bound) => {
                    launch_tree_evaluation(
                        &nested,
                        relative,
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
        "merge" if cfg.merge_image.is_some() => prepare_merge(cfg, &clean, ws, wc),
        "grep" if cfg.grep_image.is_some() => prepare_grep(cfg, &clean, ws),
        name if std_tool_image(cfg, name).is_some() => prepare_std_tool(cfg, &clean, name, ws),
        name if githist::is_builtin(name) && cfg.tools_image.is_some() => {
            prepare_githist(cfg, &clean, name, ws, wc)
        }
        name => Ok(Prepared::Result(error_block(
            &call.id,
            &format!("unavailable tool {name:?}; use run_tool with its conversation-relative path"),
        ))),
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
    if let Some(cwd) = call["input"].get("cwd").and_then(Value::as_str) {
        fs::write(dir.join("cwd"), cwd).map_err(|error| format!("writing cwd: {error}"))?;
    }
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
            (
                "current-tool",
                Arg::Lit(call["name"].as_str().unwrap_or(name)),
            ),
            ("ws", Arg::Path(ws)),
            ("tool-eval", Arg::Lit(name)),
            ("tool-args", Arg::Lit(&serialized)),
            ("tool-git", Arg::Lit(if git { "1" } else { "" })),
        ],
    )?;
    let dispatched = eval_then_catching(ws, name, Arg::Hash(&me));
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
    let source_tree_name = match target {
        Target::SourceTree { name, .. } => Some(name),
        Target::Files => None,
    };
    let commit = Oid::parse(&cas_hash(&arg("wc"))?, "evaluated tool input commit")?;
    let scoped = match &source_tree_name {
        Some(_) => cfg.clone(),
        None => cfg.clone(),
    };
    let (ws, wc) = materialize_source_tree(state, &commit)?;
    let current_input = match &source_tree_name {
        Some(name) => {
            state
                .conversation()?
                .source_tree(name)?
                .ok_or("tool source tree disappeared")?
                .commit
        }
        None => state.head().clone(),
    };
    if current_input != commit {
        return resume(cfg, state, request, request_head);
    }
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
        if let Some(refs) = scoped.merge_refs.as_deref() {
            args.push(("refs", Arg::Lit(refs)));
        }
    }
    let curried = caos_curry(Arg::Hash(&tool_tree), &args)?;
    let task_text = prepare_request(Arg::Hash(&curried), &[("in", Arg::Path(&ws))])?;
    let task = Oid::parse(&task_text, "tree tool task")?;
    let started = CallRecord {
        request: request.clone(),
        round,
        id: id.to_string(),
        name: call.name.clone(),
        declaration_message: current.declaration_message,
        source_tree_name,
        input_commit: Some(commit),
        status: CallStatus::Started,
        task: Some(task),
        result: None,
        source_tree_resolution: None,
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
    record: &CallRecord,
) -> Result<(), String> {
    let commit = record
        .input_commit
        .as_ref()
        .ok_or("compute tool.start has no input source tree")?;
    let (ws, wc) = materialize_source_tree(state, commit)?;
    let mut extras = vec![
        ("current-tool", Arg::Lit(call.name.as_str())),
        ("ws", Arg::Path(ws.as_str())),
    ];
    let clean = call_without_source_tree(call);
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
    record: &CallRecord,
) -> Result<(), String> {
    if record.source_tree_name.is_some() || record.input_commit.is_some() {
        return Err("wait_agent tool.start unexpectedly names a source tree".to_string());
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
    record: &CallRecord,
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
            let ws = fresh("merge-source-tree");
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
                .input_commit
                .as_ref()
                .ok_or("bash record has no input source tree")?;
            let proposal = mint_source_tree_commit(state, &tree, base, "bash")?;
            Ok((block, Some(proposal)))
        }
        _ => Ok((
            tools::tree_tool_result_block(&record.id, &arg("result"))?,
            None,
        )),
    }
}

fn mint_source_tree_commit(
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
    started: &CallRecord,
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
    if started.source_tree_name.is_none() {
        return complete_files_compute(state, started, base_block, &proposal);
    }
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
            .source_tree_name
            .as_ref()
            .map(|name| state.conversation()?.source_tree(name))
            .transpose()?
            .flatten()
            .map(|source_tree| source_tree.commit);
        let base = started
            .input_commit
            .as_ref()
            .ok_or("proposal tool has no input source tree")?;
        let signature = inherited_signature(state.store(), base)?;
        let resolution = reconcile(
            state.store_mut(),
            base,
            &proposal,
            current.as_ref(),
            &signature,
        )?;
        if let SourceTreeResolution::Merged { output, .. } = &resolution {
            state.push_code(output)?;
        }
        let block = if matches!(resolution, SourceTreeResolution::Conflict { .. }) {
            error_block(
                &started.id,
                &format!(
                    "source tree proposal for call {} conflicted with concurrent changes; the source tree is unchanged and proposal {} was retained",
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

type FileEdits = Vec<(String, Option<(Mode, Vec<u8>)>)>;

fn plan_file_changes<S: ObjectStore + CodeOps>(
    store: &mut S,
    changes: &[conversation_protocol::v3::tree::Change],
    current_tree: &Oid,
) -> Result<(FileEdits, Vec<String>), String> {
    use conversation_protocol::v3::tree::Snapshot;
    let mut files = Vec::new();
    let mut conflicts = Vec::new();
    for change in changes {
        let current = Snapshot::new(store, current_tree.clone())
            .entry(&change.path)
            .map(|entry| entry.map(|e| (e.mode, e.oid)));
        let current = match current {
            Ok(current) => current,
            Err(error) if error.starts_with("path ") && error.ends_with(" is a file") => {
                conflicts.push(change.path.clone());
                continue;
            }
            Err(error) => return Err(error),
        };
        if current == change.after {
            continue;
        }
        let mut after = change.after.clone();
        if current != change.before {
            if let (
                Some((Mode::Commit, before)),
                Some((Mode::Commit, proposed)),
                Some((Mode::Commit, current)),
            ) = (&change.before, &after, &current)
            {
                let signature = inherited_signature(store, before)?;
                let resolution = reconcile(store, before, proposed, Some(current), &signature)?;
                match resolution.new_pointer() {
                    Some(output) => {
                        let output = output.clone();
                        after = Some((Mode::Commit, output));
                    }
                    None if matches!(resolution, SourceTreeResolution::Conflict { .. }) => {
                        conflicts.push(change.path.clone());
                        continue;
                    }
                    None => continue,
                }
            } else {
                conflicts.push(change.path.clone());
                continue;
            }
        }
        let value = after
            .map(|(mode, oid)| {
                let bytes = if matches!(mode, Mode::Commit | Mode::Tree) {
                    oid.encode_line()
                } else {
                    store.read_blob(&oid).map_err(String::from)?
                };
                Ok::<_, String>((mode, bytes))
            })
            .transpose()?;
        files.push((change.path.clone(), value));
    }
    // Apply the command atomically: a conflicting rename or edit must not
    // leave half the shell operation installed.
    if !conflicts.is_empty() {
        files.clear();
    }
    Ok((files, conflicts))
}

fn complete_files_compute(
    state: &mut progress::State,
    started: &CallRecord,
    block: Value,
    proposal: &Oid,
) -> Result<(), String> {
    use conversation_protocol::v3::tree::diff;
    let base = started
        .input_commit
        .as_ref()
        .ok_or("tool has no input commit")?;
    let base_tree = state.store().tree_of(base)?;
    let proposed_tree = state.store().tree_of(proposal)?;
    let changes = diff(state.store(), Some(&base_tree), &proposed_tree)?;
    if changes
        .iter()
        .any(|c| c.path == ".caos" || c.path.starts_with(".caos/"))
    {
        return complete_started_failed(
            state,
            started,
            &error_block(
                &started.id,
                ".caos is protocol metadata; the shell changes were not applied",
            ),
        );
    }
    for _ in 0..32 {
        state.reload()?;
        if state
            .conversation()?
            .tool(&started.request, started.round, &started.id)?
            .is_some_and(|record| record.is_terminal())
        {
            return Ok(());
        }
        let current_tree = state.conversation()?.tree().clone();
        let (files, conflicts) = plan_file_changes(state.store_mut(), &changes, &current_tree)?;
        for (name, value) in &files {
            if let Some((Mode::Commit, bytes)) = value {
                let output = Oid::parse_line(bytes, "reconciled source tree")?;
                if !changes
                    .iter()
                    .any(|c| c.path == *name && c.after == Some((Mode::Commit, output.clone())))
                {
                    state.push_code(&output)?;
                }
            }
        }
        let mut record = completed_record(
            started,
            ToolResult::Complete {
                observation: observation_path(started),
                proposal: Some(proposal.clone()),
            },
            None,
        );
        if started.task.is_none() {
            record.input_commit = None;
        }
        record.files = files.iter().map(|(path, _)| path.clone()).collect();
        record.files_outcome = Some(FilesOutcome {
            applied: record.files.clone(),
            conflicted: conflicts.clone(),
        });
        let observation = if conflicts.is_empty() {
            block.clone()
        } else {
            error_block(&started.id, &format!(
                "Concurrent changes conflict at {}. No shell changes were applied; proposal {proposal} was retained.",
                conflicts.join(", ")))
        };
        let expected = state.head().clone();
        match state.try_append_at(
            &expected,
            tool_complete_transition(record, &observation, files)?,
        )? {
            progress::TryAppend::Appended(_) => return Ok(()),
            progress::TryAppend::HeadChanged(_) => continue,
        }
    }
    Err("conversation kept changing while applying shell result".into())
}

fn completed_record(
    started: &CallRecord,
    result: ToolResult,
    resolution: Option<SourceTreeResolution>,
) -> CallRecord {
    CallRecord {
        status: CallRecord::expected_status(&result, resolution.as_ref()),
        result: Some(result),
        source_tree_resolution: resolution,
        files: Vec::new(),
        files_outcome: None,
        ..started.clone()
    }
}

fn complete_started_failed(
    state: &mut progress::State,
    started: &CallRecord,
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

fn observation_path(record: &CallRecord) -> String {
    format!(
        "{}/observation.json",
        paths::call_payload_dir(record.request.as_str(), record.round, &record.id)
    )
}

#[allow(clippy::type_complexity)]
fn tool_complete_transition(
    record: CallRecord,
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

/// Older file calls name a source tree separately. Translate that selector once;
/// every inline tool then reads or edits the same conversation filesystem.
fn normalize_inline_call(call: &Call, target: &Target) -> Call {
    let mut normalized = call.clone();
    if let Some(input) = normalized.input.as_object_mut() {
        input.remove("source_tree");
    }
    if let Some((key, mut path)) = inline_files_path(call) {
        let explicit_root = matches!(call.name.as_str(), "read" | "ls")
            && call.input["root"]
                .as_str()
                .is_some_and(|root| !root.trim().is_empty());
        // Leave missing/empty file paths to the tools' normal validation.
        let has_path =
            call.name == "ls" || path.split('/').any(|part| !part.is_empty() && part != ".");
        if !explicit_root && has_path {
            if let Target::SourceTree { name, .. } = target {
                path = format!("{name}/{path}");
            }
        }
        normalized.input[key] = json!(path);
    }
    normalized
}

fn execute_inline(
    state: &mut progress::State,
    site: &CallSite<'_>,
    target: Target,
) -> Result<(), String> {
    let call = normalize_inline_call(site.call, &target);
    if let Some((_, relative)) = inline_files_path(&call) {
        if matches!(call.name.as_str(), "write" | "edit")
            && (relative == ".caos" || relative.starts_with(".caos/"))
        {
            return site.fail(
                state,
                ".caos is protocol metadata; use conversation commands to change it",
            );
        }
    }
    let root = materialize_files(state)?;
    let (block, new_root) = tools::execute(&call.value(), &root)?;
    let mut stub = site.stub(None);
    if let Some(root) = new_root {
        let base = state.head().clone();
        stub.input_commit = Some(base.clone());
        let tree = Oid::parse(&cas_hash(&root)?, "inline result tree")?;
        let proposal = mint_source_tree_commit(state, &tree, &base, &call.name)?;
        state.publish_commit(&proposal)?;
        complete_files_compute(state, &stub, block, &proposal)?;
    } else {
        let record = completed_record(
            &stub,
            ToolResult::Complete {
                observation: observation_path(&stub),
                proposal: None,
            },
            None,
        );
        state.append(tool_complete_transition(record, &block, Vec::new())?)?;
    }
    Ok(())
}

fn materialize_files(state: &mut progress::State) -> Result<String, String> {
    let output = fresh("conversation-files");
    caos(["get-hash", state.conversation()?.tree().as_str(), &output])?;
    Ok(output)
}

fn call_without_source_tree(call: &Call) -> Value {
    let mut input = call.input.clone();
    if let Some(object) = input.as_object_mut() {
        object.remove("source_tree");
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
    let selected_paths = match content_paths(call) {
        Ok(paths) => paths,
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

    let prompt_path = tool_arguments_path(&parent_view, request, site.round, &call.id)?;
    drop(parent_view);
    let child_id = ids::child_id(&parent_id, request, site.round, &call.id)?;
    let signature = inherited_signature(state.store(), &parent_head)?;
    let genesis = conversation_protocol::v3::oid::ensure_genesis(state.store_mut())?;
    let view = state.conversation()?;
    let mut seed = conversation_protocol::v3::tree::TreeBuilder::from(
        selected_paths.is_none().then(|| view.tree().clone()),
    );
    if let Some(paths) = &selected_paths {
        for path in paths {
            let entry = view
                .snapshot()
                .entry(path)
                .and_then(|entry| entry.ok_or_else(|| format!("no content at {path:?}")));
            match entry {
                Ok(entry) => seed.put_oid(path, entry.mode, entry.oid),
                Err(error) => {
                    drop(view);
                    site.fail(state, &error)?;
                    return Ok(true);
                }
            }
        }
    }
    seed.delete(".caos");
    let content = seed.build(state.store_mut())?;
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
        content: Some(content.clone()),
    };
    let root_tree = apply(state.store_mut(), None, &root_transition)?;
    let root = mint(
        state.store_mut(),
        &genesis,
        &root_tree,
        root_transition.kind(),
        &signature,
    )?;
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
            source_tree_resolution: None,
        },
        payloads: Vec::new(),
    };
    let prompt_head = mint_detached(state, &root, &prompt_transition, &signature)?;
    state.push_code(&prompt_head)?;
    let (configuration, child_request) =
        subagents::child_request(&child_id, &prompt_head, &cfg.system)?;

    let admit = Transition::TurnAdmit {
        record: TurnRecord {
            id: child_request.clone(),
            request_head: prompt_head.clone(),

            model: cfg.model.clone(),
            configuration: configuration.to_string(),
            round: 0,
            calls: Vec::new(),
            interjections: Vec::new(),
            status: TurnStatus::Queued,
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
    let mut stub = site.stub(None);
    stub.task = Some(relay.clone());
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
        request: child_request.clone(),
        relay: relay.clone(),
        spawn_intent: SpawnIntent {
            request: request.clone(),
            round: site.round,
            tool: call.id.clone(),
            prompt: prompt_path,
            model: cfg.model.clone(),
            configuration: configuration.to_string(),
            content,
        },
        status: TaskStatus::Pending,
        terminal_head: None,
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
    let tree = apply(state.store_mut(), Some(parent), transition)?;
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
    let started = CallRecord {
        request: site.request.clone(),
        round: site.round,
        id: call.id.clone(),
        name: call.name.clone(),
        declaration_message: site.declaration.to_string(),
        source_tree_name: None,
        input_commit: None,
        status: CallStatus::Started,
        task: Some(child.relay.clone()),
        result: None,
        source_tree_resolution: None,
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
    record: &CallRecord,
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
    if child.status == TaskStatus::Pending {
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
    Ok((
        json!({
            "child": child_id,
            "status": child_status_text(child.status),
            "terminal_head": terminal_head.as_str(),
        }),
        None,
    ))
}

/// Select ordinary content paths, never protocol metadata.
fn content_paths(call: &Call) -> Result<Option<Vec<String>>, String> {
    let Some(value) = call.input.get("paths") else {
        return Ok(None);
    };
    let paths = value
        .as_array()
        .ok_or("paths must be an array")?
        .iter()
        .map(|value| {
            let path = value.as_str().ok_or("each path must be a string")?;
            paths::validate_tree_path(path)?;
            if path == ".caos" || path.starts_with(".caos/") {
                return Err(".caos is protocol metadata".into());
            }
            Ok(path.to_string())
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Some(paths))
}

fn harvest_agent_call(state: &mut progress::State, site: &CallSite<'_>) -> Result<(), String> {
    let value = site.call.value();
    let child_id = match subagents::required_string(&value, "child", subagents::HARVEST_TOOL) {
        Ok(id) => id,
        Err(error) => return site.fail(state, &error),
    };
    let selected = match content_paths(site.call) {
        Ok(paths) => paths,
        Err(error) => return site.fail(state, &error),
    };
    let Some(child) = state.conversation()?.child(child_id)? else {
        return site.fail(state, &format!("unknown subagent {child_id}"));
    };
    let Some(terminal) = child.terminal_head else {
        return site.fail(state, "subagent is still running");
    };
    state.fetch_object(&child.initial_head)?;
    state.fetch_object(&terminal)?;
    let base = state.store().tree_of(&child.initial_head)?;
    let result = state.store().tree_of(&terminal)?;
    if let Some(paths) = &selected {
        for path in paths {
            let before =
                conversation_protocol::v3::tree::Snapshot::new(state.store(), base.clone())
                    .entry(path);
            let after =
                conversation_protocol::v3::tree::Snapshot::new(state.store(), result.clone())
                    .entry(path);
            match (before, after) {
                (Ok(None), Ok(None)) => {
                    return site.fail(state, &format!("no child content at {path:?}"))
                }
                (Err(error), _) | (_, Err(error)) => return site.fail(state, &error),
                _ => {}
            }
        }
    }
    let changes = conversation_protocol::v3::tree::diff(state.store(), Some(&base), &result)?;
    let mut tree = conversation_protocol::v3::tree::TreeBuilder::from(Some(base));
    for change in changes {
        if change.path == ".caos" || change.path.starts_with(".caos/") {
            continue;
        }
        if selected.as_ref().is_some_and(|paths| {
            !paths
                .iter()
                .any(|path| change.path == *path || change.path.starts_with(&format!("{path}/")))
        }) {
            continue;
        }
        match change.after {
            Some((mode, oid)) => tree.put_oid(&change.path, mode, oid),
            None => tree.delete(&change.path),
        }
    }
    let tree = tree.build(state.store_mut())?;
    let proposal =
        mint_source_tree_commit(state, &tree, &child.initial_head, "apply subagent content")?;
    state.push_code(&proposal)?;
    let mut stub = site.stub(None);
    stub.input_commit = Some(child.initial_head);
    complete_files_compute(
        state,
        &stub,
        result_block(
            &stub.id,
            &format!("Applied content from subagent {child_id}"),
            false,
        ),
        &proposal,
    )
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
                status: TaskStatus::Pending,
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
    if record.status == TaskStatus::Pending {
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
    site.finish(state, &block, None, None, Some(task), |stub| {
        ToolResult::Complete {
            observation: observation_path(stub),
            proposal: None,
        }
    })
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
        TaskStatus::Pending => ("pending", None),
        TaskStatus::Complete => ("complete", record.result.as_ref().map(ToString::to_string)),
        TaskStatus::Failed => ("failed", record.result.as_ref().map(ToString::to_string)),
        TaskStatus::Cancelled => ("cancelled", None),
    }
}

fn reconcile_background_tasks(state: &mut progress::State) -> Result<(), String> {
    use conversation_protocol::v3::TaskRecord;
    for task in state
        .conversation()?
        .tasks()?
        .into_iter()
        .filter(|task| task.is_pending())
    {
        match &task {
            TaskRecord::Computation(task) => {
                if task.target_ref.as_deref() != Some(state.refname()) {
                    continue;
                }
                let (_, target) = async_work::task_request(&task.task)?;
                if target != state.refname() {
                    return Err(format!(
                        "task {} targets {target}, not {}",
                        task.task,
                        state.refname()
                    ));
                }
            }
            TaskRecord::Conversation(child) => validate_child_relay(state, child)?,
        }
        if let Err(error) = async_work::dispatch(task.computation()) {
            eprintln!(
                "llm-step: could not re-admit task {} (pending): {error}",
                task.computation()
            );
        }
        timing::phase("background task recovery");
    }
    Ok(())
}

fn announce_background_tasks(
    conversation_id: &str,
    state: &mut progress::State,
) -> Result<(), String> {
    loop {
        let view = state.conversation()?;
        let notice = pending_task_notices(conversation_id, &view)?
            .into_iter()
            .next();
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
        source_tree_resolution: None,
    }
}

fn pending_task_notices(
    conversation_id: &str,
    view: &Conversation<'_>,
) -> Result<Vec<TranscriptEntry>, String> {
    use conversation_protocol::v3::TaskRecord;
    let tasks = view
        .tasks()?
        .into_iter()
        .filter(|task| !task.is_pending())
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
        // Keep notice identities stable when reading historical records.
        let (message_id, text) = match task {
            TaskRecord::Computation(task) => {
                let id = ids::protocol_id("async-notice", &json!({"task":task.task.as_str()}))?;
                let (status, result) = async_status(&task);
                (
                    id,
                    format!(
                        "Independent task {} is {status}. Its result is {}.",
                        task.task,
                        result.unwrap_or_else(|| "null".into())
                    ),
                )
            }
            TaskRecord::Conversation(child) => {
                let terminal = child
                    .terminal_head
                    .as_ref()
                    .ok_or("terminal child has no head")?;
                let id = ids::protocol_id(
                    "subagent-notice",
                    &json!({"child":child.id.as_str(),"terminal_head":terminal.as_str()}),
                )?;
                (
                    id,
                    format!(
                        "Subagent {} is {}. Its result is {terminal}.",
                        child.id,
                        child_status_text(child.status)
                    ),
                )
            }
        };
        if !existing.contains(&message_id) {
            notices.push(background_notice(conversation_id, message_id, text));
        }
    }
    Ok(notices)
}

fn child_status_text(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "running",
        TaskStatus::Complete => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn drain(state: &mut progress::State, request: &Oid) -> Result<(), String> {
    loop {
        state.reload()?;
        let record = require_request(&state.conversation()?, request)?;
        if matches!(record.status, TurnStatus::Idle | TurnStatus::Failed) {
            return finish_from_terminal(state, request);
        }
        let round = round_state(&state.conversation()?, &record)?;
        let Some(call) = round.pending.first() else {
            let result = newest_assistant_path(&state.conversation()?, request)?;
            let terminal = state.append(Transition::TurnTerminal {
                request: request.clone(),
                outcome: TurnOutcome::Idle {
                    result,
                    interrupted: true,
                },
            })?;
            reconcile_background_tasks(state)?;
            return forward_result(&terminal.commit);
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
        let Some(record) = state.conversation()?.turn(request)? else {
            return Ok(());
        };
        if matches!(record.status, TurnStatus::Idle | TurnStatus::Failed) {
            reconcile_background_tasks(state)?;
            return Ok(());
        }
        if !matches!(record.status, TurnStatus::Running | TurnStatus::Cancelling) {
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
                source_tree_resolution: None,
            },
            payloads: Vec::new(),
        })?;
        let ordinal = appended
            .ordinal
            .ok_or("failure message did not append a transcript entry")?;
        state.append(Transition::TurnTerminal {
            request: request.clone(),
            outcome: TurnOutcome::Failed {
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
        Some(TurnOutcome::Idle { .. }) => None,
        Some(TurnOutcome::Failed { error }) => {
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
        None => forward_result(&terminal),
    }
}

fn terminal_head(state: &progress::State, request: &Oid) -> Result<Oid, String> {
    terminal_head_in(state.store(), state.head(), request)
}

fn terminal_head_in(store: &dyn ObjectStore, head: &Oid, request: &Oid) -> Result<Oid, String> {
    let mut current = head.clone();
    let mut child = Conversation::open(store, &current)?.turn(request)?;
    for _ in 0..MAX_SPINE_WALK {
        let info = store.read_commit(&current).map_err(String::from)?;
        let Some(parent) = info.parents.first() else {
            break;
        };
        let parent_record = Conversation::open(store, parent)?.turn(request)?;
        let became_terminal = child.as_ref().is_some_and(|record| {
            matches!(record.status, TurnStatus::Idle | TurnStatus::Failed)
                && !parent_record.as_ref().is_some_and(|parent| {
                    matches!(parent.status, TurnStatus::Idle | TurnStatus::Failed)
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

fn forward_result(terminal: &Oid) -> Result<(), String> {
    let dir = scratch("llm-step-result")?;
    let conversation_path = fresh("terminal-conversation");
    caos(["get-hash", terminal.as_str(), &conversation_path])?;
    link(&conversation_path, dir.join("conversation"))?;
    caos(["put", path(&dir), "/cas/out"])
}

fn source_tree_paths(state: &mut progress::State) -> Result<Vec<String>, String> {
    let source_trees = state.conversation()?.source_trees()?;
    let mut paths = Vec::with_capacity(source_trees.len());
    for source_tree in source_trees.values() {
        paths.push(materialize_source_tree(state, &source_tree.commit)?.0);
    }
    Ok(paths)
}

fn registry(cfg: &Config) -> Result<Vec<Value>, String> {
    let mut registry = vec![bash_tool()];
    registry.extend(tools::declarations());
    if cfg.run_and_update_ref_image.is_some() {
        registry.extend(subagents::declarations());
        registry.push(async_work::declaration());
    }
    if cfg.grep_image.is_some() {
        registry.push(tools::grep_declaration());
    }
    if cfg.merge_image.is_some() {
        registry.push(with_source_tree(merge_tool()));
    }
    if cfg.tools_image.is_some() {
        registry.extend(githist::declarations().into_iter().map(with_source_tree));
    }
    for &(name, arg_name) in &STD_TOOLS {
        if cfg.std_tool_images.get(name).is_some_and(Option::is_some) {
            if let Some(tool) = tools::std_tool(name, &arg(arg_name))? {
                registry.push(with_source_tree(tools::tree_tool_declaration(&tool)));
            }
        }
    }
    registry.push(json!({
        "name":"run_tool",
        "description":"Run a repository tool by conversation-relative path, e.g. feature/dirty/caos-tools/test. The tool runs with the containing source tree as its input; its schema is listed in repository context.",
        "input_schema":{"type":"object","properties":{
            "path":{"type":"string"}, "arguments":{"type":"object"}
        },"required":["path"]}
    }));
    Ok(registry)
}

fn with_source_tree(mut declaration: Value) -> Value {
    if let Some(properties) = declaration
        .pointer_mut("/input_schema/properties")
        .and_then(Value::as_object_mut)
    {
        properties.insert(
            "source_tree".to_string(),
            json!({
                "type":"string",
                "description":"Conversation-relative gitlink path to operate on, e.g. feature/dirty. Required even when only one source tree exists. Paths within this Git operation are relative to that source tree."
            }),
        );
    }
    let required = declaration["input_schema"]
        .as_object_mut()
        .unwrap()
        .entry("required")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .unwrap();
    if !required.iter().any(|value| value == "source_tree") {
        required.push(json!("source_tree"));
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
        "name":"bash",
        "description":"Run sh -c from the conversation root. Ordinary files (including memories and skills) and source trees are writable together. Declare paths to read or edit existing content; undeclared content stays lazy. Use mkdir, mv, cp -a and rm to organize source trees. cp -a preserves their commit identity; editing their files creates child commits when the result is stored. Protected .caos metadata cannot be changed.",
        "input_schema":{"type":"object","properties":{
            "cmd":{"type":"string"},
            "cwd":{"type":"string","description":"Optional conversation-relative working directory; defaults to the conversation root."},
            "paths":{"type":"array","items":{"type":"string"},"description":"Conversation-relative files or directories to materialize, independent of cwd. A directory includes its descendants, including source trees."}
        },"required":["cmd"]}
    })
}

fn merge_tool() -> Value {
    json!({
        "name": "merge",
        "description": "Three-way merge another commit into the current source tree. `theirs` is a full commit hash already imported into CAOS (a custom harness may also supply a named ref snapshot); the current side is the source tree as it is now. A clean merge advances the source tree to the merged result. A conflict advances it too, with git's inline conflict markers in the files and a reserved `.caos/conflicts` file listing every unresolved path — including structural conflicts (delete/modify, mode, binary) that have NO markers. Resolve each: edit the file (use `read` with the stage's oid as `root` to inspect its content), then delete that path's rows from `.caos/conflicts`. Then build and test.",
        "input_schema": {
            "type":"object",
            "properties":{"theirs":{"type":"string","description":"Full imported commit hash. A ref name works only if the harness explicitly supplied a ref snapshot."}},
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
            "merge produced conflicts. The source tree now carries git's inline conflict markers in the affected files, plus .caos/conflicts (git's unmerged notation, richer than markers). Resolve each path — edit the file, reading a stage's content with `read` (pass the stage oid as `root`) — then delete that path's rows from .caos/conflicts. Build and test when done.\n\n.caos/conflicts:\n{}",
            body.trim_end()
        ),
        None => "merge completed cleanly; the source tree is the merged result.".to_string(),
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
            "the `{tool}` tool failed to run: {}\n\nThe source tree is unchanged. This is the tool itself failing, not a non-zero exit from your command.",
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
        GitStore, Identity, IdentityKind, MemoryStore, RefUpdate, TreeBuilder,
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
        let applied = apply_transition(store, Some(parent), &transition)?;
        let signature = inherited_signature(store, parent)?;
        mint(store, parent, &applied, transition.kind(), &signature)
    }

    fn root_with(
        store: &mut dyn ObjectStore,
        source_trees: BTreeMap<String, Oid>,
    ) -> Result<Oid, String> {
        let genesis = ensure_genesis(store)?;
        let mut content = TreeBuilder::from(None);
        for (name, commit) in source_trees {
            content.put_oid(&name, Mode::Commit, commit);
        }
        let transition = Transition::ConversationRoot {
            identity: Identity {
                id: CONVERSATION.to_string(),
                kind: IdentityKind::Root,
                owner: None,
            },
            title: "test conversation".to_string(),
            content: Some(content.build(store)?),
        };
        let applied = apply_transition(store, None, &transition)?;
        mint(
            store,
            &genesis,
            &applied,
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
        let proposal = mint_source_tree_commit(&mut state, &proposal_tree, &base, "bash").unwrap();
        assert!(state.store().has_local(&proposal_tree).unwrap());
        let resolution = reconcile(
            state.store_mut(),
            &base,
            &proposal,
            Some(&current),
            &signature,
        )
        .unwrap();
        assert!(matches!(resolution, SourceTreeResolution::Merged { .. }));

        let mut before = TreeBuilder::from(None);
        before.put_oid("code", Mode::Commit, base.clone());
        before.put("memory", Mode::Blob, b"before".to_vec());
        let before = before.build(state.store_mut()).unwrap();
        let mut proposed = TreeBuilder::from(Some(before.clone()));
        proposed.put_oid("code", Mode::Commit, proposal.clone());
        proposed.put("memory", Mode::Blob, b"after".to_vec());
        let proposed = proposed.build(state.store_mut()).unwrap();
        let mut current_files = TreeBuilder::from(Some(before.clone()));
        current_files.put_oid("code", Mode::Commit, current.clone());
        current_files.put("unrelated", Mode::Blob, b"keep".to_vec());
        let current_files = current_files.build(state.store_mut()).unwrap();
        let changes =
            conversation_protocol::v3::tree::diff(state.store(), Some(&before), &proposed).unwrap();
        let (files, conflicts) =
            plan_file_changes(state.store_mut(), &changes, &current_files).unwrap();
        assert!(conflicts.is_empty());
        assert_eq!(
            files
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["code", "memory"]
        );
        let merged = Oid::parse_line(&files[0].1.as_ref().unwrap().1, "merged code").unwrap();
        assert!(state.store().is_ancestor(&current, &merged).unwrap());
        assert!(state.store().is_ancestor(&proposal, &merged).unwrap());
        let mut collision = TreeBuilder::from(Some(current_files));
        collision.put("memory", Mode::Blob, b"concurrent".to_vec());
        let collision = collision.build(state.store_mut()).unwrap();
        let (files, conflicts) =
            plan_file_changes(state.store_mut(), &changes, &collision).unwrap();
        assert!(
            files.is_empty(),
            "a conflict must not apply half the shell result"
        );
        assert_eq!(conflicts, ["memory"]);
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
            source_tree_resolution: None,
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

        let admitted = append_memory(
            &mut store,
            &user,
            Transition::TurnAdmit {
                record: TurnRecord {
                    id: request.clone(),
                    request_head: user.clone(),

                    model: "test-model".to_string(),
                    configuration: "test-config".to_string(),
                    round: 0,
                    calls: Vec::new(),
                    interjections: Vec::new(),
                    status: TurnStatus::Queued,
                    latest_message: None,
                    escape_reason: None,
                    outcome: None,
                },
            },
        )?;
        let claimed = append_memory(
            &mut store,
            &admitted,
            Transition::TurnClaim {
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
                    source_tree_resolution: None,
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
                    paths::call_payload_dir(golden.request.as_str(), 0, "first")
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
                    paths::call_payload_dir(golden.request.as_str(), 0, "second")
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
                CallStatus::Cancelled
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
                    status: TaskStatus::Pending,
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
                status: TaskStatus::Failed,
                result: Some(result),
                reason: None,
            },
        )
        .unwrap();
        let view = Conversation::open(&store, &terminal).unwrap();
        let notices = pending_task_notices(CONVERSATION, &view).unwrap();
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
        assert!(pending_task_notices(
            CONVERSATION,
            &Conversation::open(&store, &announced).unwrap()
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn subagent_notice_is_idempotent_and_names_the_checkpoint() {
        let mut store = MemoryStore::new();
        let fork = conversation_protocol::v3::fixtures::golden(&mut store);
        let head = store.read_commit(&fork).unwrap().parents[0].clone();
        let view = Conversation::open(&store, &head).unwrap();
        let child = view.children().unwrap().into_iter().next().unwrap();
        let terminal = child.terminal_head.clone().unwrap();
        let notices = pending_task_notices("golden-conversation", &view).unwrap();
        assert_eq!(notices.len(), 2);
        let notice = notices
            .iter()
            .find(|entry| {
                entry.message_id
                    == ids::protocol_id(
                        "subagent-notice",
                        &json!({"child":child.id.as_str(),"terminal_head":terminal.as_str()}),
                    )
                    .unwrap()
            })
            .unwrap();
        assert_eq!(
            notice.blocks,
            vec![Block::Text {
                text: format!(
                    "Subagent {} is completed. Its result is {terminal}.",
                    child.id
                )
            }]
        );
        let mut announced = head.clone();
        for notice in notices {
            announced = append_memory(
                &mut store,
                &announced,
                Transition::MessageAppend {
                    entry: notice,
                    payloads: Vec::new(),
                },
            )
            .unwrap();
        }
        assert!(pending_task_notices(
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

        let admitted = append_memory(
            &mut store,
            &user,
            Transition::TurnAdmit {
                record: TurnRecord {
                    id: request.clone(),
                    request_head: user.clone(),

                    model: "test-model".to_string(),
                    configuration: "test-config".to_string(),
                    round: 0,
                    calls: Vec::new(),
                    interjections: Vec::new(),
                    status: TurnStatus::Queued,
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
            Transition::TurnEscape {
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
    fn explicit_source_tree_files_paths_normalize_to_conversation_paths() {
        let mut store = MemoryStore::default();
        let head = root_with(&mut store, BTreeMap::from([("main".into(), test_oid('a'))])).unwrap();
        let view = Conversation::open(&store, &head).unwrap();
        let browse = Call {
            id: "browse".into(),
            name: "ls".into(),
            input: json!({}),
        };
        assert!(matches!(
            resolve_target(&view, &browse).unwrap(),
            Target::Files
        ));
        let read = Call {
            id: "read".into(),
            name: "read".into(),
            input: json!({"file-path":"main/README.md"}),
        };
        assert!(matches!(
            resolve_target(&view, &read).unwrap(),
            Target::Files
        ));
        for (name, path_arg) in [
            ("read", "file-path"),
            ("write", "file-path"),
            ("edit", "file-path"),
            ("ls", "path"),
        ] {
            let mut call = Call {
                id: "call-1".into(),
                name: name.into(),
                input: json!({"source_tree":"main", path_arg:"files/config.json"}),
            };
            assert!(
                matches!(
                    resolve_target(&view, &call).unwrap(),
                    Target::SourceTree { .. }
                ),
                "{name}"
            );
            let target = resolve_target(&view, &call).unwrap();
            let normalized = normalize_inline_call(&call, &target);
            assert_eq!(normalized.input[path_arg], "main/files/config.json");
            assert!(normalized.input.get("source_tree").is_none());
            assert!(matches!(
                resolve_target(&view, &normalized).unwrap(),
                Target::Files
            ));
            if matches!(name, "read" | "ls") {
                call.input["root"] = json!(test_oid('b').as_str());
                assert_eq!(
                    normalize_inline_call(&call, &target).input[path_arg],
                    "files/config.json"
                );
                call.input.as_object_mut().unwrap().remove("root");
            }
            call.input[path_arg] = json!(".");
            assert_eq!(
                normalize_inline_call(&call, &target).input[path_arg],
                if name == "ls" { "main/." } else { "." }
            );
            call.input["source_tree"] = json!("missing");
            assert!(resolve_target(&view, &call)
                .unwrap_err()
                .contains("unknown source tree"));
            call.input.as_object_mut().unwrap().remove("source_tree");
            assert!(
                matches!(resolve_target(&view, &call).unwrap(), Target::Files),
                "{name}"
            );
        }
    }

    #[test]
    fn filesystem_tools_use_the_root_with_zero_or_multiple_source_trees() {
        let mut store = MemoryStore::new();
        for trees in [
            BTreeMap::new(),
            BTreeMap::from([("api".into(), test_oid('a'))]),
            BTreeMap::from([("api".into(), test_oid('a')), ("web".into(), test_oid('b'))]),
        ] {
            let head = root_with(&mut store, trees).unwrap();
            let view = Conversation::open(&store, &head).unwrap();
            let git_call = Call {
                id: "git".into(),
                name: "merge".into(),
                input: json!({}),
            };
            assert!(resolve_target(&view, &git_call).is_err());
            for name in ["bash", "grep", "read", "write", "ls"] {
                let call = Call {
                    id: "call".into(),
                    name: name.into(),
                    input: json!({}),
                };
                assert!(matches!(
                    resolve_target(&view, &call).unwrap(),
                    Target::Files
                ));
            }
        }
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
