//! Recording a Claude Code session as an ordinary CAOS conversation.
//!
//! Claude Code drives the model; CAOS keeps the durable log. Every hook Claude
//! Code fires arrives here as one JSON object on stdin, so the whole surface is
//! a single `caos cc hook` command and `.claude/settings.json` needs no shell
//! at all — no `jq`, no quoting, none of the constructs CLAUDE.md catalogs as
//! this tree's most reliable source of bugs. The payload names its own event
//! (`hook_event_name`), so one command serves every hook.
//!
//! The conversation these events build is an ordinary one: the same
//! `refs/caos/v2/conversations/<id>/head` ref, the same append-only spine, the
//! same events `caos tui` already replays (design/chat.md). Its id is derived
//! from the Claude Code session id rather than stored in a side table, so there
//! is no local state to lose or corrupt: the ref is the whole record.
//!
//! What this module deliberately does NOT write is lifecycle state. The
//! protocol's `queued`/`running` admission exists so a worker can claim a
//! request, and nothing here is ever claimed by a worker — Claude Code already
//! ran the turn. `fold_events` defaults an unspecified status to `idle`, so
//! omitting admission entirely is both honest and exactly what keeps
//! `caos talk` and the TUI's `reconcile_active_requests` from trying to resume
//! a request that was never dispatched.

mod serve;
mod tools;

use std::io::Read;

use serde_json::{json, Value};

use std::collections::BTreeMap;

use caos::{GitTransport, Transport};
use conversation_protocol::v3::apply::{apply, inherited_signature, mint, Transition};
use conversation_protocol::v3::git_store::GitStore;
use conversation_protocol::v3::oid::{Oid, G3};
use conversation_protocol::v3::canonical::canonical_bytes;
use conversation_protocol::v3::paths;
use conversation_protocol::v3::refs;
use conversation_protocol::v3::tree::Signature;
use conversation_protocol::v3::records::{
    Block, DeclaredCall, Identity, IdentityKind, RequestOutcome, RequestRecord, RequestStatus,
    Role, ToolRecord, ToolResult as ProtocolToolResult, ToolStatus, TranscriptEntry,
};
use conversation_protocol::v3::view::Conversation;

use crate::{
    conversation_ref, default_title, default_workspace_name, ensure_code_commit,
    fetch_validated_head, mint_transition, oid, open_store, push_cas,
    reject_reserved_caos, resolve_base, resolve_username, signature, update_local_cache,
    TurnOptions, MAX_APPEND_ATTEMPTS,
};
use tools::ToolError;

/// Conversation ids for recorded sessions live under one component so they are
/// obvious in the sidebar and cannot collide with a hand-named conversation.
/// `ConversationId::parse` accepts it: Claude Code session ids are lowercase
/// hex and dashes.
const SESSION_PREFIX: &str = "cc/";

/// Claude Code names an MCP tool `mcp__<server>__<tool>`. Only calls to our own
/// server get a session injected; everything else this hook sees is left
/// exactly as the model wrote it.
const TOOL_PREFIX: &str = "mcp__caos__";

/// What a request record says ran the turn. Claude Code chooses the model and
/// does not tell a hook which, so naming the harness is the honest answer --
/// better than a plausible-looking model string nothing verified.
const CLAUDE_CODE_MODEL: &str = "claude-code";
/// The configuration a request was admitted under. There is none to record:
/// nothing here curries a worker.
const CLAUDE_CODE_CONFIGURATION: &str = "claude-code";

/// The workspace arrives UNRESOLVED because only `serve` can carry on without
/// one. A hook that cannot find the repository has nothing to record into and
/// should say so; a tool server that cannot find it still has to answer, or
/// the session sees `CONNECTION_CLOSED` and no reason at all.
pub fn cli_cc(workspace: Result<GitTransport, String>, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("hook") => hook(&workspace?),
        Some("serve") => serve::serve(workspace),
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "usage:\n  \
     caos cc hook    (reads one Claude Code hook payload on stdin)\n  \
     caos cc serve   (workspace tool server; JSON-RPC on stdio)"
        .to_string()
}

/// Run one workspace tool and record it the way `llm-step` records one.
///
/// FOUR TRANSITIONS, in two compare-and-swaps: the model's declaration of the
/// call and the tool's start, then the tool's completion. v3 will not accept a
/// tool that no message declared (`validate_current_call`), so the declaration
/// is not bookkeeping -- it is what makes the call legible as the model's.
///
/// Recording the start BEFORE running is the protocol's first invariant
/// ("record an action before launching it", design/chat.md): a long tool is
/// visible in the tui while it runs, and a session that dies mid-call leaves a
/// record that it was attempted.
fn run_tool(
    t: &GitTransport,
    session: &str,
    name: &str,
    args: &Value,
) -> Result<String, ToolError> {
    let id = conversation_id_for(session).map_err(ToolError::Infra)?;
    let tool_use_id = args
        .get("caos_tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or(name)
        .to_string();
    let declared = declared_args(args);
    let username = resolve_username(t, None).map_err(ToolError::Infra)?;

    // The record this call will be completed against, captured from the
    // attempt that won the compare-and-swap.
    let mut started: Option<ToolRecord> = None;
    let start = append(t, &id, |store, head| {
        let view = Conversation::open(store, head)?;
        let Some(request) = view.active_request()? else {
            return Err(format!(
                "no active request in {id:?}: a tool call arrived before this session's prompt"
            ));
        };
        let ordinal = view.transcript_len()?;
        let workspaces = view.workspace_names()?;
        let workspace = workspaces.first().cloned();
        let input = match &workspace {
            Some(name) => view.workspace(name)?.map(|record| record.commit),
            None => None,
        };
        drop(view);

        let round = request.round;
        let signature = inherited_signature(store, head)?;
        let message_id = caos::fresh_entropy()?;
        let dir = paths::transcript_payload_dir(ordinal, &message_id);
        let admitted = paths::admit_external_id(&tool_use_id);
        let payload_name = format!("args-{admitted}.json");
        let entry = TranscriptEntry {
            message_id: message_id.clone(),
            conversation: id.to_string(),
            role: Role::Assistant,
            actor: username.clone(),
            request: Some(request.id.clone()),
            round: Some(round),
            model: Some(CLAUDE_CODE_MODEL.to_string()),
            blocks: vec![Block::ToolUse {
                id: tool_use_id.clone(),
                name: name.to_string(),
                arguments: format!("{dir}/{payload_name}"),
            }],
            proposal: None,
            workspace_resolution: None,
        };
        let declaration = mint_transition(
            store,
            head,
            &Transition::ModelComplete {
                request: request.id.clone(),
                entry,
                payloads: vec![(payload_name, payload_bytes(&declared)?)],
                calls: vec![DeclaredCall {
                    id: tool_use_id.clone(),
                    name: name.to_string(),
                }],
            },
            &signature,
        )?;

        let record = ToolRecord {
            request: request.id.clone(),
            round,
            id: tool_use_id.clone(),
            name: name.to_string(),
            declaration_message: message_id,
            workspace_name: workspace,
            input_workspace: input,
            status: ToolStatus::Started,
            // NOT A DISPATCHED TASK. Every other harness points this at the
            // sub-run it launched; cc runs its tools in-process, so there is
            // nothing to point at. The field is required, so it carries a
            // content-derived oid: an object that exists, is stable for a
            // retry of the same call, and claims nothing false.
            task: Some(call_oid(t, &id, &tool_use_id)?),
            result: None,
            workspace_resolution: None,
            files: Vec::new(),
            files_outcome: None,
        };
        started = Some(record.clone());
        Ok(Some(mint_transition(
            store,
            &declaration,
            &Transition::ToolStart { record },
            &signature,
        )?))
    });
    start.map_err(ToolError::Infra)?;
    let started = started.ok_or_else(|| ToolError::Infra("the tool never started".to_string()))?;

    // Run it against the workspace commit the start recorded.
    let input = started
        .input_workspace
        .as_ref()
        .ok_or_else(|| ToolError::Infra("the conversation has no workspace".to_string()))?;
    let run = tools::execute(t, input.as_str(), name, args);
    let (text, is_error, produced) = match &run {
        Ok(outcome) => (outcome.text.clone(), false, outcome.commit.clone()),
        Err(ToolError::User(message)) => (message.clone(), true, None),
        Err(ToolError::Infra(error)) => return Err(ToolError::Infra(error.clone())),
    };

    let observation = paths::tool_payload_dir(started.request.as_str(), started.round, &started.id);
    let completed = ToolRecord {
        status: match is_error {
            false => ToolStatus::Complete,
            true => ToolStatus::Failed,
        },
        result: Some(match is_error {
            false => ProtocolToolResult::Complete {
                observation: format!("{observation}/observation.json"),
                proposal: produced.as_deref().map(str::to_string).and_then(|commit| {
                    Oid::parse(&commit, "tool workspace commit").ok()
                }),
            },
            true => ProtocolToolResult::Failed {
                error: text.clone(),
            },
        }),
        ..started.clone()
    };
    let payloads = match is_error {
        false => vec![(
            "observation.json".to_string(),
            payload_bytes(&json!({ "text": text }))
                .map_err(ToolError::Infra)?,
        )],
        true => Vec::new(),
    };
    append(t, &id, |store, head| {
        let signature = inherited_signature(store, head)?;
        Ok(Some(mint_transition(
            store,
            head,
            &Transition::ToolComplete {
                record: completed.clone(),
                payloads: payloads.clone(),
                files: Vec::new(),
            },
            &signature,
        )?))
    })
    .map_err(ToolError::Infra)?;

    run.map(|_| text)
}

/// A stable oid for one tool call, standing in for a task cc never dispatched.
fn call_oid(t: &GitTransport, id: &str, tool_use_id: &str) -> Result<Oid, String> {
    let seed = format!("{id}\0{tool_use_id}");
    let hash = t.put_object("blob", seed.as_bytes())?;
    oid(&hash.to_string(), "tool task id")
}


/// A stable 40-hex request id for the turn this call belongs to.
///
/// The protocol requires `request` to be a canonical object id and the fold
/// validates it, so this hashes the prompt id into a git blob — deterministic,
/// dependency-free, and the resulting object actually resolves. It names a turn,
/// exactly as `llm-step`'s `run` does; nothing dispatches it, because nothing
/// here ever writes `queued` or `running`.
fn turn_request(t: &GitTransport, session: &str, args: &Value) -> Result<String, ToolError> {
    let seed = match args.get("caos_prompt_id").and_then(Value::as_str) {
        Some(prompt) => format!("{session}\0{prompt}"),
        // No prompt id means the PreToolUse hook is older than this field.
        // Falling back to the session keeps every call in one turn-shaped scope
        // rather than failing a tool over a presentation detail.
        None => session.to_string(),
    };
    t.put_object("blob", seed.as_bytes())
        .map(|oid| oid.to_string())
        .map_err(ToolError::Infra)
}

/// The model's own arguments, without the values the hook injected: those are
/// plumbing, and showing them in the transcript would misrepresent the call the
/// model actually made.
fn declared_args(args: &Value) -> Value {
    let mut declared = args.clone();
    if let Some(object) = declared.as_object_mut() {
        object.remove("caos_session");
        object.remove("caos_tool_use_id");
        object.remove("caos_prompt_id");
    }
    declared
}


/// Dispatch one hook payload. An event we do not record is not an error: Claude
/// Code fires many, and a settings file that routes extra ones here should keep
/// working rather than failing a turn.
fn hook(t: &GitTransport) -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| format!("reading hook payload: {error}"))?;
    let payload: Value =
        serde_json::from_str(&input).map_err(|error| format!("parsing hook payload: {error}"))?;
    let event = string_field(&payload, "hook_event_name")?;
    match event {
        "UserPromptSubmit" => on_user_prompt(t, &payload),
        "PreToolUse" => on_pre_tool_use(&payload),
        "Stop" => on_stop(t, &payload),
        "StopFailure" => on_stop_failure(t, &payload),
        _ => Ok(()),
    }
}

/// The user's prompt, and the only event allowed to create the conversation:
/// the first prompt of a session establishes its base and fallback title
/// exactly as the TUI's first message does.
fn on_user_prompt(t: &GitTransport, payload: &Value) -> Result<(), String> {
    let id = conversation_id(payload)?;
    let prompt = string_field(payload, "prompt")?;
    if prompt.trim().is_empty() {
        return Ok(());
    }
    record_prompt(t, &id, prompt)
}

/// Record a user's prompt, creating the conversation on the session's first one.
///
/// THREE TRANSITIONS, minted in one compare-and-swap: the message, the request
/// it opens, and the claim that puts the request in `running`. v3 requires the
/// last of those before any tool may be recorded (`require_request_running_or_-
/// cancelling`), and Claude Code has already begun the turn by the time this
/// hook fires -- so admitting and claiming in one step is not a shortcut, it is
/// the truth: nothing is ever going to poll for this request.
fn record_prompt(t: &GitTransport, id: &str, prompt: &str) -> Result<(), String> {
    let options = TurnOptions::default();
    let username = resolve_username(t, None)?;
    let signature = signature(&username)?;
    let refname = conversation_ref(id)?;

    for _ in 0..MAX_APPEND_ATTEMPTS {
        let mut store = open_store(t)?;
        let observed = fetch_validated_head(t, &store, id)?.map(|(_, head)| head);
        let head = match &observed {
            Some(head) => head.clone(),
            None => root_commit(t, &mut store, id, prompt, &options, &signature)?,
        };

        // The message. Its id is fresh entropy, as the client's own is: a
        // transcript entry is addressed by it, and two prompts in one session
        // must not collide.
        let message_id = caos::fresh_entropy()?;
        let entry = TranscriptEntry {
            message_id: message_id.clone(),
            conversation: id.to_string(),
            role: Role::User,
            actor: username.clone(),
            request: None,
            round: None,
            model: None,
            blocks: vec![Block::Text {
                text: prompt.to_string(),
            }],
            proposal: None,
            workspace_resolution: None,
        };
        let message = mint_transition(
            &mut store,
            &head,
            &Transition::MessageAppend {
                entry,
                payloads: Vec::new(),
            },
            &signature,
        )?;

        // The request. Its id is CONTENT, not a name: the session and the
        // message together, hashed, so re-running this hook for the same prompt
        // names the same request rather than opening a second one.
        let request = request_oid(t, id, &message_id)?;
        let view = Conversation::open(&store, &message)?;
        let workspaces = view.workspaces_tree()?;
        drop(view);
        let record = RequestRecord {
            id: request.clone(),
            request_head: message.clone(),
            request_workspaces: workspaces,
            model: CLAUDE_CODE_MODEL.to_string(),
            configuration: CLAUDE_CODE_CONFIGURATION.to_string(),
            round: 0,
            calls: Vec::new(),
            interjections: Vec::new(),
            status: RequestStatus::Queued,
            latest_message: None,
            escape_reason: None,
            outcome: None,
        };
        let admission = inherited_signature(&store, &message)?;
        let admitted = mint_transition(
            &mut store,
            &message,
            &Transition::RequestAdmit { record },
            &admission,
        )?;
        let claimed = mint_transition(
            &mut store,
            &admitted,
            &Transition::RequestClaim {
                request,
                latest_message: message_id,
            },
            &admission,
        )?;

        if push_cas(&store, &refname, observed.as_ref(), &claimed)? {
            let _ = update_local_cache(t, &refname, claimed.as_str());
            return Ok(());
        }
    }
    Err(format!(
        "conversation {id:?} head moved during all {MAX_APPEND_ATTEMPTS} append attempts"
    ))
}

/// The conversation's root commit, for a session whose first prompt this is.
fn root_commit(
    t: &GitTransport,
    store: &mut GitStore,
    id: &str,
    prompt: &str,
    options: &TurnOptions,
    signature: &Signature,
) -> Result<Oid, String> {
    let base = oid(&resolve_base(t, options)?, "conversation base")?;
    ensure_code_commit(t, store, &base)?;
    reject_reserved_caos(t, base.as_str(), "base workspace")?;
    let workspace = default_workspace_name(t, options)?;
    let genesis = oid(G3, "v3 genesis")?;
    let root = Transition::ConversationRoot {
        identity: Identity {
            id: id.to_string(),
            kind: IdentityKind::Root,
            owner: None,
        },
        title: default_title(prompt),
        workspaces: BTreeMap::from([(workspace, (base, None))]),
        files_seed: None,
    };
    let tree = apply(store, None, &root)?.tree;
    mint(store, &genesis, &tree, root.kind(), signature)
}

/// A request id derived from the session and message rather than dispatched.
///
/// The protocol wants an Oid that resolves, and nothing here prepares a worker
/// request to supply one -- Claude Code ran the turn. Hashing the pair into a
/// blob gives an object that exists, is stable for a retry of the same prompt,
/// and cannot collide with another session's.
fn request_oid(t: &GitTransport, id: &str, message_id: &str) -> Result<Oid, String> {
    let seed = format!("{id}\0{message_id}");
    let hash = t.put_object("blob", seed.as_bytes())?;
    oid(&hash.to_string(), "request id")
}

/// Tell a caos workspace tool which conversation it is working in.
///
/// The tool server is spawned once per session and is otherwise stateless, so
/// this is the only thing that attributes a call to a conversation. Both values
/// are declared in every tool's schema rather than smuggled in, so the model's
/// own call stays schema-valid and this hook only fills in values the tool
/// already accepted.
///
/// No `permissionDecision` is emitted. Injection and permission are separate
/// concerns: allowing these tools without a prompt is a choice for
/// `permissions.allow`, not something a hook should decide on the user's behalf
/// just because it happened to be in the call path.
fn on_pre_tool_use(payload: &Value) -> Result<(), String> {
    let tool = string_field(payload, "tool_name")?;
    if !tool.starts_with(TOOL_PREFIX) {
        return Ok(());
    }
    let session = string_field(payload, "session_id")?;
    let mut input = payload
        .get("tool_input")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let Some(object) = input.as_object_mut() else {
        return Err(format!("{tool} was called with a non-object tool_input"));
    };
    object.insert("caos_session".to_string(), json!(session));
    if let Some(id) = payload.get("tool_use_id").and_then(Value::as_str) {
        object.insert("caos_tool_use_id".to_string(), json!(id));
    }
    // The turn this call belongs to, which becomes the event's `request` — the
    // same job `llm-step`'s `run` does for a turn it drives itself.
    if let Some(prompt) = payload.get("prompt_id").and_then(Value::as_str) {
        object.insert("caos_prompt_id".to_string(), json!(prompt));
    }
    let response = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": input,
        }
    });
    println!(
        "{}",
        serde_json::to_string(&response)
            .map_err(|error| format!("encoding hook response: {error}"))?
    );
    Ok(())
}



/// Whether this event may bring a conversation into existence. Only the user's
/// prompt may: an assistant or lifecycle event arriving for an unknown
/// conversation means hooks were installed mid-session or the ref was deleted
/// under us, and inventing a root from it would silently produce a conversation
/// whose transcript begins in the middle.
enum Creation {
    Allowed { title: String },
    Refused,
}


/// A session's conversation id is derived, never stored: the ref is the only
/// state, so there is no map to fall out of step with the sessions it names.
fn conversation_id(payload: &Value) -> Result<String, String> {
    conversation_id_for(string_field(payload, "session_id")?)
}

/// A session id is the one ref component we do not author, so it is validated
/// through the protocol's own parser rather than trusted — whether it arrives
/// in a hook payload or as a tool argument.
fn conversation_id_for(session: &str) -> Result<String, String> {
    let id = format!("{SESSION_PREFIX}{session}");
    refs::validate_conversation_id(&id)?;
    Ok(id)
}

fn string_field<'a>(payload: &'a Value, key: &str) -> Result<&'a str, String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("hook payload has no string {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_become_valid_conversation_ids() {
        let payload = json!({"session_id": "88888abc-9ae5-4d07-a44d-54b366776bdc"});
        assert_eq!(
            conversation_id(&payload).unwrap(),
            "cc/88888abc-9ae5-4d07-a44d-54b366776bdc"
        );
    }

    /// A session id is the one part of the ref path we do not author, so it is
    /// validated rather than trusted: the protocol's own parser is what decides
    /// whether it can name a ref.
    #[test]
    fn hostile_session_ids_are_refused_before_naming_a_ref() {
        for hostile in ["../../etc", "a/../b", "with space", "", "head", "a.lock"] {
            let payload = json!({ "session_id": hostile });
            assert!(
                conversation_id(&payload).is_err(),
                "accepted session id {hostile:?}"
            );
        }
    }

    #[test]
    fn a_payload_without_its_event_name_is_an_error() {
        assert!(string_field(&json!({}), "hook_event_name").is_err());
        assert!(string_field(&json!({"hook_event_name": 2}), "hook_event_name").is_err());
    }
}

/// Append whatever `step` mints, compare-and-swapping the conversation head.
///
/// `step` returns the new head, or `None` when there is nothing to record --
/// a hook that fires for a turn this session never opened, say. The retries are
/// not a concurrency model: the tool server handles one call at a time. They
/// protect against ANOTHER writer, such as an interjection typed into the tui.
fn append(
    t: &GitTransport,
    id: &str,
    mut step: impl FnMut(&mut GitStore, &Oid) -> Result<Option<Oid>, String>,
) -> Result<(), String> {
    let refname = conversation_ref(id)?;
    for _ in 0..MAX_APPEND_ATTEMPTS {
        let mut store = open_store(t)?;
        let Some((_, head)) = fetch_validated_head(t, &store, id)? else {
            return Err(format!(
                "no conversation {id:?} to record into; \
                 a session's first recorded event is its user prompt"
            ));
        };
        let Some(candidate) = step(&mut store, &head)? else {
            return Ok(());
        };
        if push_cas(&store, &refname, Some(&head), &candidate)? {
            let _ = update_local_cache(t, &refname, candidate.as_str());
            return Ok(());
        }
    }
    Err(format!(
        "conversation {id:?} head moved during all {MAX_APPEND_ATTEMPTS} append attempts"
    ))
}

/// A payload's bytes, in the protocol's canonical framing.
fn payload_bytes(value: &Value) -> Result<Vec<u8>, String> {
    const PREFIX: &[u8] = b"{\"payload\":";
    let wrapped = canonical_bytes(&json!({ "payload": value }))?;
    if !wrapped.starts_with(PREFIX) || !wrapped.ends_with(b"}\n") {
        return Err("canonical payload wrapper had an unexpected shape".to_string());
    }
    Ok(wrapped[PREFIX.len()..wrapped.len() - 2].to_vec())
}

/// The assistant's closing message, and the end of the request it answered.
fn on_stop(t: &GitTransport, payload: &Value) -> Result<(), String> {
    let id = conversation_id(payload)?;
    let message = payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let username = resolve_username(t, None)?;
    let conversation = id.clone();
    append(t, &id, move |store, head| {
        let _ = &conversation;
        let view = Conversation::open(store, head)?;
        let Some(request) = view.active_request()? else {
            // Nothing to close. A Stop for a turn that opened no request is
            // not an error: an interrupted session can leave one behind.
            return Ok(None);
        };
        let ordinal = view.transcript_len()?;
        let round = request.round;
        drop(view);

        let signature = inherited_signature(store, head)?;
        let mut at = head.clone();
        if !message.trim().is_empty() {
            let message_id = caos::fresh_entropy()?;
            let dir = paths::transcript_payload_dir(ordinal, &message_id);
            let entry = TranscriptEntry {
                message_id,
                conversation: conversation.clone(),
                role: Role::Assistant,
                actor: username.clone(),
                request: Some(request.id.clone()),
                round: Some(round),
                model: Some(CLAUDE_CODE_MODEL.to_string()),
                blocks: vec![Block::Text {
                    text: message.clone(),
                }],
                proposal: None,
                workspace_resolution: None,
            };
            let _ = &dir;
            at = mint_transition(
                store,
                &at,
                &Transition::ModelComplete {
                    request: request.id.clone(),
                    entry,
                    payloads: Vec::new(),
                    calls: Vec::new(),
                },
                &signature,
            )?;
        }
        let terminal = mint_transition(
            store,
            &at,
            &Transition::RequestTerminal {
                request: request.id.clone(),
                outcome: RequestOutcome::Idle {
                    result: None,
                    interrupted: false,
                },
            },
            &signature,
        )?;
        Ok(Some(terminal))
    })
}

/// A turn that ended badly closes its request as failed.
fn on_stop_failure(t: &GitTransport, payload: &Value) -> Result<(), String> {
    let id = conversation_id(payload)?;
    let kind = payload
        .get("error_type")
        .and_then(Value::as_str)
        .unwrap_or("error");
    let detail = payload
        .get("error_message")
        .and_then(Value::as_str)
        .unwrap_or("");
    let error = match detail.trim().is_empty() {
        true => kind.to_string(),
        false => format!("{kind}: {detail}"),
    };
    append(t, &id, move |store, head| {
        let view = Conversation::open(store, head)?;
        let Some(request) = view.active_request()? else {
            return Ok(None);
        };
        drop(view);
        let signature = inherited_signature(store, head)?;
        let terminal = mint_transition(
            store,
            head,
            &Transition::RequestTerminal {
                request: request.id,
                outcome: RequestOutcome::Failed {
                    error: error.clone(),
                },
            },
            &signature,
        )?;
        Ok(Some(terminal))
    })
}
