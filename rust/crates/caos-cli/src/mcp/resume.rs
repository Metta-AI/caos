//! Picking a recorded conversation up in a NEW Claude Code session.
//!
//! Two commands, both named by the conversation COMMIT they start from (the hash
//! `caos head <hash>` printed after a turn -- a commit is immutable, where a ref
//! moves):
//!
//! ```text
//! /fork-caos-conversation <hash>          a NEW branch: the new session's own
//!                                         `cc/<session>` conversation, whose
//!                                         history is <hash>'s, then diverges
//! /resume-from-caos-conversation <hash>   the SAME branch: the new session
//!                                         appends to the conversation <hash>
//!                                         belongs to
//! ```
//!
//! Both are recognised by the `UserPromptSubmit` hook on a session's first
//! prompt, so the conversation exists -- and holds the earlier history and
//! workspace -- before the model takes its first turn. What the MODEL sees of
//! that history is the slash command's own text (see `cloud/install.go`); this
//! file is the record side.
//!
//! A fork needs no bookkeeping: its id is derived from the session id like any
//! other. A resume does: the conversation keeps its OLD id, so the session has to
//! be told which one it is writing to. That is a small file under the checkout's
//! git directory, read wherever a session id becomes a conversation id, and it
//! is the one piece of local state in `caos mcp`. Losing it (a fresh container
//! for the same session) is safe in the way that matters: the session then
//! records into its own derived conversation rather than into someone else's.
//!
//! THE HEAD IS USED AS IT IS, EVEN IF IT IS INCOMPLETE. A session that died
//! mid-turn leaves a running request and calls that will never finish; the
//! protocol refuses to fork such a conversation and a resumed session could never
//! close it. `settle` closes them the way `llm-step` does for a cancelled turn --
//! each open call becomes `cancelled`, the request ends `interrupted` -- as new
//! commits on top of the head. Nothing is rewritten, and the transcript says
//! plainly which results never arrived.

use std::path::PathBuf;

use serde_json::{json, Value};

use caos::GitTransport;
use conversation_protocol::v3::apply::{inherited_signature, Transition};
use conversation_protocol::v3::canonical::canonical_bytes;
use conversation_protocol::v3::git_store::GitStore;
use conversation_protocol::v3::oid::Oid;
use conversation_protocol::v3::paths;
use conversation_protocol::v3::records::{
    CallRecord, CallStatus, Role, TaskStatus, ToolResult, TurnOutcome, TurnStatus,
};
use conversation_protocol::v3::refs;
use conversation_protocol::v3::view::Conversation;

use crate::{
    conversation_ref, fetch_validated_head, mint_transition, open_store, oid, push_cas,
    resolve_username, update_local_cache, validate_cached,
};

pub(super) const RESUME_COMMAND: &str = "resume-from-caos-conversation";
pub(super) const FORK_COMMAND: &str = "fork-caos-conversation";

/// A line the slash commands' expanded text carries, so the hook still finds the
/// request if Claude Code hands it the expansion rather than what was typed.
/// `caos-conversation-command: resume <hash>` or `... fork <hash>`.
pub(super) const MARKER: &str = "caos-conversation-command:";

/// Why a call or request a session left open was closed.
const ABANDONED: &str =
    "the session that ran this call ended before it finished, so its result was never recorded";

/// Far more than a conversation can have open (one request, one round of calls,
/// its background tasks); it exists so a bug cannot loop forever.
const MAX_SETTLE_STEPS: usize = 1024;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    Resume(Oid),
    Fork(Oid),
}

/// The command a prompt asks for, if it asks for one.
///
/// The slash form counts only as the FIRST line of the prompt: a prompt that
/// merely quotes one further down is not a request to fork anything. The marker
/// form may be anywhere, because it is text the command itself expanded to.
pub(super) fn parse_command(prompt: &str) -> Result<Option<Command>, String> {
    let trimmed = prompt.trim_start();
    if let Some(first) = trimmed.lines().next().and_then(|l| l.strip_prefix('/')) {
        let mut words = first.split_whitespace();
        let resume = match words.next() {
            Some(RESUME_COMMAND) => Some(true),
            Some(FORK_COMMAND) => Some(false),
            _ => None,
        };
        if let Some(resume) = resume {
            return command(resume, words.next()).map(Some);
        }
    }
    for line in prompt.lines() {
        let Some(rest) = line.trim().strip_prefix(MARKER) else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let resume = match words.next() {
            Some("resume") => true,
            Some("fork") => false,
            other => return Err(format!("unknown {MARKER} mode {other:?}")),
        };
        return command(resume, words.next()).map(Some);
    }
    Ok(None)
}

fn command(resume: bool, hash: Option<&str>) -> Result<Command, String> {
    let name = if resume { RESUME_COMMAND } else { FORK_COMMAND };
    let hash = hash.ok_or_else(|| format!("usage: /{name} <conversation-commit-hash>"))?;
    if hash.len() != 40 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "/{name} takes a full 40-character commit hash (the one `caos head` printed), not {hash:?}"
        ));
    }
    let hash = oid(&hash.to_ascii_lowercase(), "conversation commit")?;
    Ok(if resume {
        Command::Resume(hash)
    } else {
        Command::Fork(hash)
    })
}

// --- which conversation a session writes to ---------------------------------

fn mapping_path(t: &GitTransport, session: &str) -> PathBuf {
    t.git_dir()
        .join("caos-cc-sessions")
        .join(paths::admit_external_id(session))
}

/// The conversation a RESUMED session was pointed at, if it was.
fn mapped(t: &GitTransport, session: &str) -> Option<String> {
    let text = std::fs::read_to_string(mapping_path(t, session)).ok()?;
    let id = text.trim();
    (!id.is_empty()).then(|| id.to_string())
}

fn remember(t: &GitTransport, session: &str, id: &str) -> Result<(), String> {
    let path = mapping_path(t, session);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("creating {}: {error}", dir.display()))?;
    }
    std::fs::write(&path, format!("{id}\n"))
        .map_err(|error| format!("writing {}: {error}", path.display()))
}

/// The conversation a session records into: the one it was resumed onto, or the
/// one its id derives.
pub(super) fn conversation_for_session(t: &GitTransport, session: &str) -> Result<String, String> {
    match mapped(t, session) {
        Some(id) => {
            refs::validate_conversation_id(&id)?;
            Ok(id)
        }
        None => super::conversation_id_for(session),
    }
}

// --- the two commands -------------------------------------------------------

/// Do what a session's first prompt asked, returning a note for the person
/// running it, or `None` when there was nothing to do (the session already has
/// its conversation, so this is not its first prompt).
pub(super) fn begin(
    t: &GitTransport,
    session: &str,
    command: &Command,
) -> Result<Option<String>, String> {
    let user = resolve_username(t, None)?;
    match command {
        Command::Fork(from) => fork(t, session, &user, from),
        Command::Resume(from) => resume(t, session, from),
    }
}

/// A new branch: the session's own conversation, made by forking `from` after
/// settling it. The original is untouched.
fn fork(t: &GitTransport, session: &str, user: &str, from: &Oid) -> Result<Option<String>, String> {
    let id = super::conversation_id_for(session)?;
    let mut store = open_store(t)?;
    if fetch_validated_head(t, &store, &id)?.is_some() {
        return Ok(None);
    }
    store.ensure_local(from)?;
    validate_cached(&store, from)?;
    let settled = settle(&mut store, from)?;
    let title = format!("fork of {}", &from.as_str()[..12]);
    // `settled` is `from` itself when nothing was open, so this is exactly the
    // TUI's fork in the common case.
    crate::fork_conversation(t, user, &id, &title, settled.as_str())?;
    Ok(Some(format!(
        "caos conversation ref {} forked from {from}",
        conversation_ref(&id)?
    )))
}

/// The same branch: the session appends to the conversation `from` belongs to.
///
/// `from` must BE that conversation's head. A commit further back would have the
/// session append to a branch that has since moved, and a silent divergence is
/// worse than a refusal that names the fork command.
fn resume(t: &GitTransport, session: &str, from: &Oid) -> Result<Option<String>, String> {
    if mapped(t, session).is_some() {
        return Ok(None);
    }
    let mut store = open_store(t)?;
    store.ensure_local(from)?;
    validate_cached(&store, from)?;
    let id = Conversation::open(&store, from)?.identity()?.id;
    let refname = refs::head_ref(&id)?;
    let head = store
        .read_ref(&refname)?
        .ok_or_else(|| format!("conversation {id:?} has no head on this caos server"))?;
    if head != *from {
        return Err(format!(
            "{from} is not the head of conversation {id:?}, which is at {head}. \
             /{RESUME_COMMAND} continues a branch from its latest commit; \
             use /{FORK_COMMAND} to start a new branch from this one"
        ));
    }
    let settled = settle(&mut store, from)?;
    if settled != *from {
        if !push_cas(&store, &refname, Some(from), &settled)? {
            return Err(format!(
                "conversation {id:?} moved while it was being resumed; \
                 run the command again with its new head"
            ));
        }
        let _ = update_local_cache(t, &refname, settled.as_str());
    }
    remember(t, session, &id)?;
    Ok(Some(format!(
        "caos conversation ref {} resumed at {settled}",
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
    fn the_slash_forms_are_recognised_on_the_first_line() {
        let want = |resume: bool| Some(command(resume, Some(HASH)).unwrap());
        assert_eq!(
            parse_command(&format!("/{RESUME_COMMAND} {HASH}")).unwrap(),
            want(true)
        );
        assert_eq!(
            parse_command(&format!("  /{FORK_COMMAND}   {HASH}\nmore text")).unwrap(),
            want(false)
        );
    }

    #[test]
    fn the_expanded_marker_is_recognised_anywhere() {
        let prompt = format!("Some preamble.\n{MARKER} fork {HASH}\nmore.");
        assert_eq!(
            parse_command(&prompt).unwrap(),
            Some(command(false, Some(HASH)).unwrap())
        );
    }

    #[test]
    fn other_prompts_and_builtin_commands_are_not_ours() {
        assert_eq!(parse_command("fix the bug").unwrap(), None);
        // Claude Code's own /resume must not be mistaken for ours.
        assert_eq!(parse_command(&format!("/resume {HASH}")).unwrap(), None);
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
            format!("/{RESUME_COMMAND} {}", &HASH[..12]),
        ] {
            let error = parse_command(&prompt).unwrap_err();
            assert!(error.contains("/fork-caos-conversation") || error.contains("/resume-from"));
        }
        assert!(parse_command(&format!("{MARKER} sideways {HASH}")).is_err());
    }
}
