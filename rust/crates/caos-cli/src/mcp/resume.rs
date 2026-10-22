//! Picking a recorded conversation up in a NEW Claude Code session.
//!
//! One command, named by the conversation COMMIT it starts from (the hash
//! `caos head <hash>` printed after a turn -- a commit is immutable, where a ref
//! moves):
//!
//! ```text
//! /fork-caos-conversation <hash>     a NEW branch: the new session's own
//!                                    `cc/<session>` conversation, whose
//!                                    history is <hash>'s, then diverges
//! ```
//!
//! It is recognised by the `UserPromptSubmit` hook on a session's first prompt,
//! so the conversation exists -- and holds the earlier history and workspace --
//! before the model takes its first turn. What the MODEL sees of that history is
//! the slash command's own text (see `cloud/install.go`); this file is the record
//! side.
//!
//! A fork needs no bookkeeping: its id is derived from the session id like any
//! other, so a session's conversation is always `conversation_id_for(session)`.
//!
//! THE HEAD IS USED AS IT IS, EVEN IF IT IS INCOMPLETE. A session that died
//! mid-turn leaves a running request and calls that will never finish; the
//! protocol refuses to fork such a conversation. `settle` closes them the way
//! `llm-step` does for a cancelled turn -- each open call becomes `cancelled`,
//! the request ends `interrupted` -- as new commits on top of the head. Nothing
//! is rewritten, and the transcript says plainly which results never arrived.

use serde_json::{json, Value};

use conversation_protocol::v3::apply::{inherited_signature, Transition};
use conversation_protocol::v3::canonical::canonical_bytes;
use conversation_protocol::v3::git_store::GitStore;
use conversation_protocol::v3::oid::Oid;
use conversation_protocol::v3::paths;
use conversation_protocol::v3::records::{
    CallRecord, CallStatus, Role, TaskStatus, ToolResult, TurnOutcome, TurnStatus,
};
use conversation_protocol::v3::view::Conversation;

use conversation_protocol::v3::refs;

use crate::{conversation_ref, mint_transition, oid};

pub(super) const FORK_COMMAND: &str = "fork-caos-conversation";

/// A line the slash command's expanded text carries, so the hook still finds the
/// request if Claude Code hands it the expansion rather than what was typed.
/// `caos-conversation-command: fork <hash>`.
pub(super) const MARKER: &str = "caos-conversation-command:";

/// Why a call or request a session left open was closed.
const ABANDONED: &str =
    "the session that ran this call ended before it finished, so its result was never recorded";

/// Far more than a conversation can have open (one request, one round of calls,
/// its background tasks); it exists so a bug cannot loop forever.
const MAX_SETTLE_STEPS: usize = 1024;

/// The commit a prompt asks to fork, if it asks for one.
///
/// The slash form counts only as the FIRST line of the prompt: a prompt that
/// merely quotes one further down is not a request to fork anything. The marker
/// form may be anywhere, because it is text the command itself expanded to.
pub(super) fn parse_command(prompt: &str) -> Result<Option<Oid>, String> {
    let trimmed = prompt.trim_start();
    if let Some(first) = trimmed.lines().next().and_then(|l| l.strip_prefix('/')) {
        let mut words = first.split_whitespace();
        if words.next() == Some(FORK_COMMAND) {
            return commit(words.next()).map(Some);
        }
    }
    for line in prompt.lines() {
        let Some(rest) = line.trim().strip_prefix(MARKER) else {
            continue;
        };
        let mut words = rest.split_whitespace();
        return match words.next() {
            Some("fork") => commit(words.next()).map(Some),
            other => Err(format!("unknown {MARKER} mode {other:?}")),
        };
    }
    Ok(None)
}

fn commit(hash: Option<&str>) -> Result<Oid, String> {
    let hash = hash.ok_or_else(|| format!("usage: /{FORK_COMMAND} <conversation-commit-hash>"))?;
    if hash.len() != 40 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "/{FORK_COMMAND} takes a full 40-character commit hash (the one `caos head` printed), not {hash:?}"
        ));
    }
    oid(&hash.to_ascii_lowercase(), "conversation commit")
}

// --- the command ------------------------------------------------------------

/// A new branch: the session's own conversation, made by forking `from` after
/// settling it. The original is untouched. `None` when the session already has
/// its conversation, so this is not its first prompt.
pub(super) fn begin(server: &str, session: &str, from: &Oid) -> Result<Option<String>, String> {
    let user = super::username()?;
    let id = super::conversation_id_for(session)?;
    let mut store = super::server_store(server)?;
    if store.read_ref(&refs::head_ref(&id)?)?.is_some() {
        return Ok(None);
    }
    store.ensure_local(from)?;
    let settled = settle(&mut store, from)?;
    let title = format!("fork of {}", &from.as_str()[..12]);
    // `settled` is `from` itself when nothing was open, so this is exactly the
    // TUI's fork in the common case.
    crate::fork_conversation_in(&mut store, &user, &id, &title, settled.as_str())?;
    Ok(Some(format!(
        "caos conversation ref {} forked from {from}",
        conversation_ref(&id)?
    )))
}

// --- closing what a dead session left open ----------------------------------

enum Action {
    /// A request that never started: escape it.
    Escape(Oid),
    /// A call that started and never finished, or was declared and never started.
    Cancel(Box<CallRecord>),
    /// Every call is closed; end the request.
    Terminal {
        request: Oid,
        result: Option<String>,
    },
    /// A background computation nobody is waiting on any more.
    Async(Oid),
    Done,
}

/// Commits, on top of `head`, that leave the conversation with nothing open. It is
/// `head` itself when nothing was.
///
/// The same closing `llm-step` does for a cancelled request (its `drain`): each
/// open call is completed as `cancelled` with an error observation, then the
/// request ends `interrupted`. What it does not touch is a pending publication --
/// that is a fact about a remote, not something to guess at -- so a conversation
/// holding one is still refused, by the protocol, with its own message.
pub(super) fn settle(store: &mut GitStore, head: &Oid) -> Result<Oid, String> {
    let mut head = head.clone();
    for _ in 0..MAX_SETTLE_STEPS {
        let action = {
            let view = Conversation::open(&*store, &head)?;
            next_action(&view)?
        };
        let transition = match action {
            Action::Done => return Ok(head),
            Action::Escape(request) => Transition::TurnEscape {
                request,
                reason: Some(ABANDONED.to_string()),
            },
            Action::Cancel(open) => cancel(*open)?,
            Action::Terminal { request, result } => Transition::TurnTerminal {
                request,
                outcome: TurnOutcome::Idle {
                    result,
                    interrupted: true,
                },
            },
            Action::Async(task) => Transition::AsyncTerminal {
                task,
                status: TaskStatus::Cancelled,
                result: None,
                reason: Some(ABANDONED.to_string()),
            },
        };
        let signature = inherited_signature(&*store, &head)?;
        head = mint_transition(store, &head, &transition, &signature)?;
    }
    Err(format!(
        "could not settle {head} in {MAX_SETTLE_STEPS} steps; the conversation keeps opening work"
    ))
}

fn next_action(view: &Conversation<'_>) -> Result<Action, String> {
    if let Some(request) = view.active_turn()? {
        if request.status == TurnStatus::Queued {
            return Ok(Action::Escape(request.id));
        }
        // Calls that STARTED and never finished, in any round.
        for round in 0..request.round {
            if let Some(open) = view
                .tools(&request.id, round)?
                .into_iter()
                .find(|call| !call.is_terminal())
            {
                return Ok(Action::Cancel(Box::new(open)));
            }
        }
        // Calls the current round DECLARED and nothing started. They block no
        // fork, but a declared call with no result is a hole the transcript
        // would carry forever, so they are closed too.
        if let Some(declaring) = request.round.checked_sub(1) {
            for call in &request.calls {
                if view.tool(&request.id, declaring, &call.id)?.is_none() {
                    return Ok(Action::Cancel(Box::new(CallRecord {
                        request: request.id.clone(),
                        round: declaring,
                        id: call.id.clone(),
                        name: call.name.clone(),
                        declaration_message: declaring_message(view, &request.id, declaring)?,
                        source_tree_name: None,
                        input_commit: None,
                        status: CallStatus::Cancelled,
                        task: None,
                        result: None,
                        source_tree_resolution: None,
                        files: Vec::new(),
                        files_outcome: None,
                    })));
                }
            }
        }
        let result = newest_assistant_path(view, &request.id)?;
        return Ok(Action::Terminal {
            request: request.id,
            result,
        });
    }
    for task in view.async_tasks()? {
        if task.status == TaskStatus::Pending {
            return Ok(Action::Async(task.task));
        }
    }
    Ok(Action::Done)
}

/// The id of the assistant message that declared `round`'s calls: what a call
/// record calls its `declaration_message`.
fn declaring_message(view: &Conversation<'_>, request: &Oid, round: u64) -> Result<String, String> {
    for ordinal in (0..view.transcript_len()?).rev() {
        if let Some((_, entry)) = view.transcript_entry(ordinal)? {
            if entry.role == Role::Assistant
                && entry.request.as_ref() == Some(request)
                && entry.round == Some(round)
            {
                return Ok(entry.message_id);
            }
        }
    }
    Err(format!(
        "request {request} round {round} has no assistant declaration"
    ))
}

fn newest_assistant_path(view: &Conversation<'_>, request: &Oid) -> Result<Option<String>, String> {
    for ordinal in (0..view.transcript_len()?).rev() {
        if let Some((_, entry)) = view.transcript_entry(ordinal)? {
            if entry.role == Role::Assistant && entry.request.as_ref() == Some(request) {
                return Ok(Some(paths::transcript_entry_path(
                    ordinal,
                    &entry.message_id,
                )));
            }
        }
    }
    Ok(None)
}

/// Complete `open` as cancelled, with the same observation shape `llm-step`
/// records for a call it cancels, so anything that renders a transcript renders
/// this one like any other.
fn cancel(open: CallRecord) -> Result<Transition, String> {
    let block = json!({
        "type": "tool_result",
        "tool_use_id": open.id,
        "content": [{"type": "text", "text": ABANDONED}],
        "is_error": true,
    });
    let record = CallRecord {
        status: CallStatus::Cancelled,
        result: Some(ToolResult::Cancelled {
            reason: ABANDONED.to_string(),
        }),
        source_tree_resolution: None,
        files: Vec::new(),
        files_outcome: None,
        ..open
    };
    Ok(Transition::ToolComplete {
        record,
        payloads: vec![("observation.json".to_string(), observation_bytes(&block)?)],
        files: Vec::new(),
    })
}

/// A payload in the protocol's canonical framing, trailing newline included --
/// byte for byte what `llm-step`'s `canonical_payload_bytes` writes.
fn observation_bytes(value: &Value) -> Result<Vec<u8>, String> {
    const PREFIX: &[u8] = b"{\"payload\":";
    let wrapped = canonical_bytes(&json!({ "payload": value }))?;
    if !wrapped.starts_with(PREFIX) || !wrapped.ends_with(b"}\n") {
        return Err("canonical payload wrapper had an unexpected shape".to_string());
    }
    let mut bytes = wrapped[PREFIX.len()..wrapped.len() - 2].to_vec();
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn the_slash_form_is_recognised_on_the_first_line() {
        let want = Some(commit(Some(HASH)).unwrap());
        assert_eq!(
            parse_command(&format!("  /{FORK_COMMAND}   {HASH}\nmore text")).unwrap(),
            want
        );
    }

    #[test]
    fn the_expanded_marker_is_recognised_anywhere() {
        let prompt = format!("Some preamble.\n{MARKER} fork {HASH}\nmore.");
        assert_eq!(
            parse_command(&prompt).unwrap(),
            Some(commit(Some(HASH)).unwrap())
        );
    }

    #[test]
    fn other_prompts_and_builtin_commands_are_not_ours() {
        assert_eq!(parse_command("fix the bug").unwrap(), None);
        // Claude Code's own /resume must not be mistaken for ours.
        assert_eq!(parse_command(&format!("/resume {HASH}")).unwrap(), None);
        assert_eq!(
            parse_command(&format!("/resume-caos-conversation {HASH}")).unwrap(),
            None
        );
        // Quoting a command further down is not asking for it.
        assert_eq!(
            parse_command(&format!("look:\n/{FORK_COMMAND} {HASH}")).unwrap(),
            None
        );
    }

    #[test]
    fn a_bad_hash_is_an_error_naming_the_command() {
        for prompt in [
            format!("/{FORK_COMMAND}"),
            format!("/{FORK_COMMAND} main"),
            format!("/{FORK_COMMAND} {}", &HASH[..12]),
        ] {
            let error = parse_command(&prompt).unwrap_err();
            assert!(error.contains("/fork-caos-conversation"));
        }
        assert!(parse_command(&format!("{MARKER} sideways {HASH}")).is_err());
        assert!(parse_command(&format!("{MARKER} resume {HASH}")).is_err());
    }
}
