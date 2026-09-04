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

use caos::{GitTransport, Transport};
use conversation_protocol::ConversationId;

use crate::{
    conversation_ref, create_event_commit, default_title, fetch_conversation_commit, push_head_cas,
    reject_reserved_caos, remote_ref, resolve_base, resolve_username,
    try_push_initial_conversation, update_local_cache, TurnOptions, MAX_APPEND_ATTEMPTS,
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
/// TWO events, not one: the call before the tool runs, the result after. That
/// is the protocol's first invariant ("record an action before launching it and
/// a result before consuming it", design/chat.md) and what `llm-step` does — so
/// a long tool is visible in the tui while it runs instead of appearing only
/// once it finishes, and a session that dies mid-call leaves a record that it
/// was attempted.
///
/// Execution is serial, also matching `llm-step`'s single queue: the tool server
/// reads and handles one JSON-RPC request at a time, so a batch of parallel
/// calls from the model executes one after another, each starting from the head
/// the previous one left. The compare-and-swap retries below are therefore not a
/// concurrency model — they are protection against ANOTHER writer, such as an
/// interjection typed into the tui against the same conversation.
fn run_tool(
    t: &GitTransport,
    session: &str,
    name: &str,
    args: &Value,
) -> Result<String, ToolError> {
    let id = conversation_id_for(session).map_err(ToolError::Infra)?;
    let refname = conversation_ref(&id).map_err(ToolError::Infra)?;
    let tool_use_id = args
        .get("caos_tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or(name);
    let request = turn_request(t, session, args)?;
    let declared = declared_args(args);

    append_tool_event(
        t,
        &refname,
        &id,
        None,
        json!({
            "request": request,
            "round": ROUND,
            "author": "assistant",
            "content": "",
            "calls": [{ "id": tool_use_id, "name": name, "args": declared }],
        }),
    )?;

    let mut outcome = None;
    let tree = append_tool_event(
        t,
        &refname,
        &id,
        Some(&mut |workspace: &str| {
            let run = match tools::execute(t, workspace, name, args) {
                Ok(produced) => Ok(produced),
                Err(ToolError::User(message)) => Err(message),
                Err(ToolError::Infra(error)) => return Err(ToolError::Infra(error)),
            };
            let (text, tree) = match &run {
                Ok(produced) => (
                    produced.text.clone(),
                    produced
                        .tree
                        .clone()
                        .unwrap_or_else(|| workspace.to_string()),
                ),
                Err(message) => (message.clone(), workspace.to_string()),
            };
            let is_error = run.is_err();
            outcome = Some(run.map(|_| text.clone()).map_err(ToolError::User));
            Ok((
                tree,
                json!({
                    "request": request,
                    "round": ROUND,
                    "result": {
                        "tool_use_id": tool_use_id,
                        "content": [{ "type": "text", "text": text }],
                        "is_error": is_error,
                    },
                }),
            ))
        }),
        Value::Null,
    )?;
    let _ = tree;
    outcome.ok_or_else(|| ToolError::Infra("the tool produced no outcome".to_string()))?
}

/// Claude Code does not expose the model's round number, and it does not need
/// to: the fold keys tool activity on `(request, round, tool_use_id)`, a
/// `tool_use_id` is unique for the whole session, and a call and its result
/// share all three. So one round per turn pairs exactly as `llm-step`'s calls do
/// WITHIN a round, which is the only place the number does any work.
const ROUND: u64 = 0;

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

/// Append one tool event at the conversation head, retrying the protocol's
/// compare-and-swap when another writer wins.
///
/// With `produce`, the event and its workspace tree are computed INSIDE the
/// loop, so a retry re-reads the new tree and applies the tool to it rather than
/// committing a result derived from a tree that no longer exists. Without one,
/// the event is tree-neutral and simply moves to the new head.
#[allow(clippy::type_complexity)]
fn append_tool_event(
    t: &GitTransport,
    refname: &str,
    id: &str,
    mut produce: Option<&mut dyn FnMut(&str) -> Result<(String, Value), ToolError>>,
    event: Value,
) -> Result<String, ToolError> {
    for _ in 0..MAX_APPEND_ATTEMPTS {
        let head = remote_ref(t, refname)
            .map_err(ToolError::Infra)?
            .ok_or_else(|| {
                ToolError::Infra(format!(
                    "no conversation {id:?} to work in; \
                     the UserPromptSubmit hook records a session's first event"
                ))
            })?;
        fetch_conversation_commit(t, refname, &head).map_err(ToolError::Infra)?;
        let workspace = t
            .git_capture(&["rev-parse", &format!("{head}^{{tree}}")], None)
            .map_err(ToolError::Infra)?
            .trim()
            .to_string();
        let (tree, event) = match produce.as_mut() {
            Some(produce) => produce(&workspace)?,
            None => (workspace, event.clone()),
        };
        let commit = create_event_commit(t, &tree, &head, &event).map_err(ToolError::Infra)?;
        if push_head_cas(t, refname, Some(&head), &commit).map_err(ToolError::Infra)? {
            let _ = update_local_cache(t, refname, &commit);
            return Ok(tree);
        }
    }
    Err(ToolError::Infra(format!(
        "conversation {id:?} head moved during all {MAX_APPEND_ATTEMPTS} attempts"
    )))
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
    let options = TurnOptions::default();
    let username = resolve_username(t, None)?;
    append_event(
        t,
        &options,
        &id,
        json!({
            "author": "user",
            "username": username,
            "content": prompt,
        }),
        Creation::Allowed {
            title: default_title(prompt),
        },
    )
    .map(|_| ())
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

/// The assistant's closing text for the turn. A turn that ended without any
/// text — every round spent in tool calls — records nothing rather than an
/// empty message, matching the fold's own treatment of empty content.
fn on_stop(t: &GitTransport, payload: &Value) -> Result<(), String> {
    let id = conversation_id(payload)?;
    let message = payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .unwrap_or("");
    if message.trim().is_empty() {
        return Ok(());
    }
    append_event(
        t,
        &TurnOptions::default(),
        &id,
        json!({
            "author": "assistant",
            "content": message,
        }),
        Creation::Refused,
    )
    .map(|_| ())
}

/// A turn that died in the API rather than finishing. This is the one place a
/// recorded conversation carries a status: `failed` is real information, and
/// the fold retires it as soon as any later event supersedes it.
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
    append_event(
        t,
        &TurnOptions::default(),
        &id,
        json!({ "status": "failed", "error": error }),
        Creation::Refused,
    )
    .map(|_| ())
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

/// Append one event to the conversation's canonical head, retrying the exact
/// compare-and-swap the protocol requires when a concurrent writer wins.
///
/// The tree is the parent's: nothing in this phase changes a workspace, because
/// Claude Code's tools do not yet run through CAOS. When they do, the mutating
/// ones supply their own tree here.
fn append_event(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    event: Value,
    creation: Creation,
) -> Result<String, String> {
    let refname = conversation_ref(id)?;
    let username = resolve_username(t, options.username.as_deref())?;
    for _ in 0..MAX_APPEND_ATTEMPTS {
        let observed = remote_ref(t, &refname)?;
        let title = match (&creation, observed.is_some()) {
            (Creation::Refused, false) => {
                return Err(format!(
                    "no conversation {id:?} to record into; \
                     a session's first recorded event must be its user prompt"
                ))
            }
            (Creation::Allowed { title }, false) => Some(title.as_str()),
            _ => None,
        };
        let parent = match observed.as_deref() {
            Some(head) => {
                fetch_conversation_commit(t, &refname, head)?;
                head.to_string()
            }
            None => resolve_base(t, options)?,
        };
        let tree = t
            .git_capture(&["rev-parse", &format!("{parent}^{{tree}}")], None)?
            .trim()
            .to_string();
        let mut event = event.clone();
        if let Some(title) = title {
            reject_reserved_caos(t, &tree, "base tree")?;
            event["base"] = Value::String(parent.clone());
            event["title"] = Value::String(title.to_string());
        }
        let commit = create_event_commit(t, &tree, &parent, &event)?;
        let pushed = match (observed.as_deref(), title) {
            (Some(observed), _) => push_head_cas(t, &refname, Some(observed), &commit)?,
            (None, Some(title)) => {
                try_push_initial_conversation(t, &username, id, &refname, &commit, title)?
            }
            // `title` is Some whenever the ref was absent, by the match above.
            (None, None) => unreachable!("creation refused without an observed head"),
        };
        if pushed {
            // The remote CAS is the durability boundary; the local ref is a
            // cache another client in this checkout may hold locked.
            let _ = update_local_cache(t, &refname, &commit);
            return Ok(commit);
        }
    }
    Err(format!(
        "conversation {id:?} head moved during all {MAX_APPEND_ATTEMPTS} append attempts"
    ))
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
    ConversationId::parse(&id)?;
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
