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

use std::io::Read;

use serde_json::{json, Value};

use caos::GitTransport;
use conversation_protocol::ConversationId;

use crate::{
    conversation_ref, create_event_commit, default_title, fetch_conversation_commit, push_head_cas,
    reject_reserved_caos, remote_ref, resolve_base, resolve_username,
    try_push_initial_conversation, update_local_cache, TurnOptions, MAX_APPEND_ATTEMPTS,
};

/// Conversation ids for recorded sessions live under one component so they are
/// obvious in the sidebar and cannot collide with a hand-named conversation.
/// `ConversationId::parse` accepts it: Claude Code session ids are lowercase
/// hex and dashes.
const SESSION_PREFIX: &str = "cc/";

pub fn cli_cc(t: &GitTransport, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("hook") => hook(t),
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "usage:\n  caos cc hook   (reads one Claude Code hook payload on stdin)".to_string()
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
    let session = string_field(payload, "session_id")?;
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
