//! Closing what a dead session left open, so its conversation can be forked.
//!
//! The protocol refuses to fork a conversation with a running request, and a
//! session that died mid-turn leaves one, with calls that will never finish.
//! [`settle`] appends commits that leave nothing open, as new commits on top of
//! the head: nothing is rewritten, and the transcript says plainly which results
//! never arrived.

use serde_json::{json, Value};

use conversation_protocol::v3::apply::{apply, inherited_signature, mint, Transition};
use conversation_protocol::v3::canonical::canonical_bytes;
use conversation_protocol::v3::git_store::GitStore;
use conversation_protocol::v3::view::Conversation;
use conversation_protocol::v3::{
    paths, CallRecord, CallStatus, Oid, Role, Signature, TaskStatus, ToolResult, TurnOutcome,
    TurnStatus,
};

/// Why a call or request a session left open was closed.
const ABANDONED: &str =
    "the session that ran this call ended before it finished, so its result was never recorded";

/// Far more than a conversation can have open (one request, one round of calls,
/// its background tasks); it exists so a bug cannot loop forever.
const MAX_SETTLE_STEPS: usize = 1024;

fn mint_transition(
    store: &mut GitStore,
    parent: &Oid,
    transition: &Transition,
    signature: &Signature,
) -> Result<Oid, String> {
    let applied = apply(store, Some(parent), transition)?;
    mint(store, parent, &applied, transition.kind(), signature)
}

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
pub(crate) fn settle(store: &mut GitStore, head: &Oid) -> Result<Oid, String> {
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

