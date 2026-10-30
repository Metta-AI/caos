//! `--record=<prompt|stop|failure>`: write a harness's events on a conversation.
//!
//! A harness that drives the model itself (Claude Code, Hermes) reports three
//! things to a conversation: the prompt a person typed, the message that ended
//! a turn, and a turn that ended badly. Each is one fact appended to the
//! transcript, and nothing else: the conversation keeps no turn, round or
//! request state, so there is nothing to open, hold or close. A harness that
//! backgrounds a slow call and carries on is recorded the same way as one that
//! does not -- the call's `tool_use` and its result are facts appended whenever
//! they happen.
//!
//! EVERY EVENT CARRIES ITS OWN MESSAGE ID, chosen by the caller. A repeat of an
//! event (a retried hook, a request answered twice) names the id the first one
//! recorded, finds it in the transcript and writes nothing, so recording is
//! idempotent without the step keeping any state of its own.
//!
//! The result is the conversation as it now stands (`conversation`), and
//! `status` saying what happened:
//!
//! - `created`: the first prompt, which made the conversation
//! - `forked`: the first prompt, which made the conversation as a fork of `from`
//! - `recorded`: the event was written (or had been already)
//! - `nothing`: a stop with no closing message to record

use std::time::{SystemTime, UNIX_EPOCH};

use conversation_protocol::v3::apply::{apply, client_signature, mint, mint_with_parents, Transition};
use conversation_protocol::v3::oid::ensure_genesis;
use conversation_protocol::v3::reconcile::MergeOutcome;
use conversation_protocol::v3::tree::{ObjectStore, TreeBuilder};
use conversation_protocol::v3::view::Conversation;
use conversation_protocol::v3::{
    paths, Block, GitStore, Identity, IdentityKind, Oid, RefUpdate, Role, Signature,
    TranscriptEntry,
};
use worker_common::{read_arg, read_arg_opt};

use crate::progress::{self, State, TryAppend};
use crate::{forward_files, message_ordinal};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Prompt,
    Stop,
    Failure,
}

#[derive(Clone)]
pub(crate) struct Request {
    kind: Kind,
    /// The id of the transcript entry this event writes.
    message_id: String,
    /// The prompt, the closing message, or the error.
    text: String,
    actor: String,
    /// The model the harness's request is recorded as having been answered by.
    /// A prompt's: a stop and a failure take the request's own.
    model: String,
    /// A git locator naming the COMMIT a NEW conversation's content comes from
    /// (`content-locator`). A root parents it and starts from its tree; a fork
    /// merges it into the conversation it forks.
    content: Option<String>,
    /// A conversation commit to FORK, for a NEW conversation: the prompt's
    /// conversation is then made as a fork of it, as it stands, rather than as
    /// a root.
    from: Option<Oid>,
}

impl Request {
    pub(crate) fn read() -> Result<Option<Self>, String> {
        let Some(kind) = read_arg_opt("record")? else {
            return Ok(None);
        };
        let kind = match kind.trim() {
            "prompt" => Kind::Prompt,
            "stop" => Kind::Stop,
            "failure" => Kind::Failure,
            other => return Err(format!("--record is prompt, stop or failure, not {other:?}")),
        };
        let message_id = read_arg("message-id")?.trim().to_string();
        conversation_protocol::v3::ids::validate_client_key(&message_id)
            .map_err(|error| format!("--message-id: {error}"))?;
        let content = read_arg_opt("content-locator")?
            .map(|locator| locator.trim().to_string())
            .filter(|locator| !locator.is_empty());
        let from = match read_arg_opt("from")? {
            Some(hash) => Some(Oid::parse(hash.trim(), "--from")?),
            None => None,
        };
        Ok(Some(Self {
            kind,
            message_id,
            text: read_arg_opt("text")?.unwrap_or_default(),
            actor: read_arg("actor")?.trim().to_string(),
            model: read_arg_opt("model")?.unwrap_or_default().trim().to_string(),
            content,
            from,
        }))
    }
}

pub(crate) fn run(conversation: &str, request: &Request) -> Result<(), String> {
    let mut state = loop {
        match State::open_if_present(conversation)? {
            Some(state) => break state,
            None if request.kind == Kind::Prompt => {
                // Another prompt may create it between our look and our push; the
                // loser opens the winner's.
                if let Some(state) = create(conversation, request)? {
                    return forward(&state, if request.from.is_some() { "forked" } else { "created" });
                }
            }
            None => {
                return Err(format!(
                    "no conversation {conversation:?} to record into; \
                     a session's first recorded event is its user prompt"
                ))
            }
        }
    };
    let status = match request.kind {
        Kind::Prompt => prompt(conversation, &mut state, request)?,
        Kind::Stop => stop(conversation, &mut state, request)?,
        Kind::Failure => failure(conversation, &mut state, request)?,
    };
    forward(&state, status)
}

fn forward(state: &State, status: &str) -> Result<(), String> {
    forward_files(
        state.head(),
        &[
            ("status", status.as_bytes()),
            ("head", state.head().as_str().as_bytes()),
        ],
    )
}

fn now() -> Result<i64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .map_err(|error| format!("reading the clock: {error}"))
}

fn signature(actor: &str) -> Result<Signature, String> {
    Ok(client_signature(actor, &format!("{actor}@caos"), now()?))
}

/// The same title the tui gives a conversation from its first message.
fn title(prompt: &str) -> String {
    const MAX_CHARS: usize = 60;
    let compact = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        compact
    } else {
        compact
            .chars()
            .take(MAX_CHARS - 1)
            .chain(std::iter::once('…'))
            .collect()
    }
}

/// The transcript entry this event writes: the prompt, the closing message or
/// the failure, authored by the harness's actor.
fn entry(conversation: &str, request: &Request, role: Role) -> TranscriptEntry {
    TranscriptEntry {
        message_id: request.message_id.clone(),
        conversation: conversation.to_string(),
        role,
        actor: request.actor.clone(),
        model: (role == Role::Assistant && !request.model.is_empty())
            .then(|| request.model.clone()),
        blocks: vec![Block::Text {
            text: request.text.clone(),
        }],
        proposal: None,
        source_tree_resolution: None,
    }
}

/// The prompt, minted on top of `parent` and not yet pushed.
fn mint_prompt(
    store: &mut GitStore,
    parent: &Oid,
    conversation: &str,
    request: &Request,
    signature: &Signature,
) -> Result<Oid, String> {
    let transition = Transition::MessageAppend {
        entry: entry(conversation, request, Role::User),
        payloads: Vec::new(),
    };
    let applied = apply(store, Some(parent), &transition)?;
    mint(store, parent, &applied, transition.kind(), signature)
}

/// The commit a content locator names, in `store` with its history. The server
/// is asked first: a `git+caos://` locator names it, and any commit a
/// conversation was made from before is still there. Otherwise the commit is
/// fetched from the repository the locator names, with the GitHub token the
/// `import-source` tool would use, and the push that follows carries its history
/// to the server. A locator pins its commit, so the answer is the rev it names.
fn content_commit(store: &GitStore, locator: &str) -> Result<Oid, String> {
    let parsed = git_locator::parse_git_ref(locator)?;
    let rev = parsed
        .rev
        .as_deref()
        .ok_or_else(|| format!("content locator {locator:?} pins no commit"))?;
    let commit = Oid::parse(rev, "content commit")?;
    if store.has_local(&commit)? || store.fetch_object(&commit).is_ok() {
        return Ok(commit);
    }
    if parsed.url.starts_with("git+caos://") {
        return Err(format!(
            "content locator {locator:?} names a commit this server does not have"
        ));
    }
    let url = parsed.fetch_url();
    let token = crate::import_source::token_file(&url)
        .map(std::fs::read_to_string)
        .transpose()
        .map_err(|_| "reading the GitHub token".to_string())?;
    store
        .fetch_commit_from(&url, &commit, token.as_deref().map(str::trim_end))
        .map_err(|error| format!("fetching {locator}: {error}"))?;
    Ok(commit)
}

/// then the prompt's request, pushed together with the creator's membership
/// refs. `None` when the conversation exists, so the caller records onto it.
///
/// `from` is forked as it stands. A conversation is a flat transcript, so a
/// source that died mid-turn needs no settling first; its unfinished calls are
/// tool_use blocks with no result, carried into the fork as they are.
fn create_fork(conversation: &str, request: &Request, from: &Oid) -> Result<Option<State>, String> {
    let refname = conversation_protocol::v3::refs::head_ref(conversation)?;
    let mut store = State::scratch_store()?;
    if store.fetch_ref(&refname)?.is_some() {
        return Ok(None);
    }
    store.ensure_local(from)?;
    if Conversation::open(&store, from)?.identity()?.id == conversation {
        return Err("a fork's id must differ from its source conversation".to_string());
    }
    let signature = signature(&request.actor)?;
    // With a content locator the fork starts from the MERGE of its source and
    // that commit, as git merges them: the base is what the two share, which the
    // root's parent gives them. .caos/ is the fork's own record and stays as the
    // source left it. A conflict refuses the fork, naming the files: nobody is
    // in the conversation yet to resolve it.
    let (content, merged) = match &request.content {
        Some(locator) => {
            let commit = content_commit(&store, locator)?;
            let tree = match store.merge_commits(from, &commit)? {
                MergeOutcome::Merged { tree } => tree,
                MergeOutcome::Conflict { paths } => {
                    return Err(format!(
                        "merging {locator} into the fork conflicts in {}; \
                         fork without it, or resolve that in the repository first",
                        paths.join(", ")
                    ))
                }
            };
            let mut content = TreeBuilder::from(Some(tree));
            content.delete(paths::CAOS_DIR);
            (Some(content.build(&mut store)?), vec![commit])
        }
        None => (None, Vec::new()),
    };
    let transition = Transition::ConversationFork {
        identity: Identity {
            id: conversation.to_string(),
            kind: IdentityKind::Fork {
                source: from.clone(),
            },
            owner: None,
        },
        title: format!("fork of {}", &from.as_str()[..12]),
        content,
    };
    let applied = apply(&mut store, Some(from), &transition)?;
    let forked = mint_with_parents(
        &mut store,
        from,
        &merged,
        &applied,
        transition.kind(),
        &signature,
    )?;
    let mut state = State::from_store(store, refname.clone(), forked.clone())?;
    let claimed = mint_prompt(state.store_mut(), &forked, conversation, request, &signature)?;
    let active = conversation_protocol::v3::refs::active_membership_ref(&request.actor, conversation)?;
    let archived =
        conversation_protocol::v3::refs::archived_membership_ref(&request.actor, conversation)?;
    // Negotiated: the content commit's history is on the server under no
    // advertised ref, and only negotiation keeps it out of the pack.
    let pushed = state.store().push_negotiating(&[
        RefUpdate {
            refname: refname.clone(),
            expected: None,
            new: Some(claimed.clone()),
        },
        RefUpdate {
            refname: active,
            expected: None,
            new: Some(forked),
        },
        RefUpdate {
            refname: archived,
            expected: None,
            new: None,
        },
    ]);
    match pushed {
        Ok(()) => State::from_store(state.into_store(), refname, claimed).map(Some),
        Err(error) => {
            // Lost the race to create it, or the push failed outright.
            if state.store().fetch_ref(&refname)?.is_some() {
                Ok(None)
            } else {
                Err(format!("forking into {refname}: {error}"))
            }
        }
    }
}

/// Make the conversation, with its first prompt: root, message, admission and
/// claim, pushed together to a ref that must not exist. `None` when it does, so
/// the caller records onto the winner's.
fn create(conversation: &str, request: &Request) -> Result<Option<State>, String> {
    if let Some(from) = &request.from {
        return create_fork(conversation, request, from);
    }
    let refname = conversation_protocol::v3::refs::head_ref(conversation)?;
    let mut store = State::scratch_store()?;
    // The root PARENTS the commit its content came from, so that a fork made
    // later can merge that repository's newer commit in with git finding the
    // base. Without content it parents G3, the genesis every conversation shares.
    let (parent, content) = match &request.content {
        Some(locator) => {
            let commit = content_commit(&store, locator)?;
            let tree = store.read_commit(&commit).map_err(String::from)?.tree;
            (commit, Some(tree))
        }
        None => (ensure_genesis(&mut store)?, None),
    };
    let signature = signature(&request.actor)?;
    let root = Transition::ConversationRoot {
        identity: Identity {
            id: conversation.to_string(),
            kind: IdentityKind::Root,
            owner: None,
        },
        title: title(&request.text),
        content,
    };
    let applied = apply(&mut store, None, &root)?;
    let root = mint(&mut store, &parent, &applied, root.kind(), &signature)?;
    // The chain is minted in a State whose head is the root, so it can use the
    // same helpers as a conversation that exists; only the push differs.
    let mut state = State::from_store(store, refname.clone(), root.clone())?;
    let claimed = mint_prompt(state.store_mut(), &root, conversation, request, &signature)?;
    // the server under no advertised ref.
    let pushed = state.store().push_negotiating(&[RefUpdate {
        refname: refname.clone(),
        expected: None,
        new: Some(claimed.clone()),
    }]);
    match pushed {
        Ok(()) => State::from_store(state.into_store(), refname, claimed).map(Some),
        Err(error) => {
            // Lost the race to create it, or the push failed outright.
            if state.store().fetch_ref(&refname)?.is_some() {
                Ok(None)
            } else {
                Err(format!("creating {refname}: {error}"))
            }
        }
    }
}

fn prompt(conversation: &str, state: &mut State, request: &Request) -> Result<&'static str, String> {
    append(conversation, state, request, Role::User, "prompt")
}

/// The message that ended a turn. A harness that moved a call to the background
/// ends its turn with that call still running; its result is a fact recorded
/// whenever it arrives, so nothing here waits for it or keeps anything open.
fn stop(conversation: &str, state: &mut State, request: &Request) -> Result<&'static str, String> {
    if request.text.trim().is_empty() {
        return Ok("nothing");
    }
    append(conversation, state, request, Role::Assistant, "stop")
}

fn failure(
    conversation: &str,
    state: &mut State,
    request: &Request,
) -> Result<&'static str, String> {
    append(conversation, state, request, Role::System, "failure")
}

/// Append the event's entry, once: a repeat finds its message id already there.
fn append(
    conversation: &str,
    state: &mut State,
    request: &Request,
    role: Role,
    what: &str,
) -> Result<&'static str, String> {
    for _ in 0..progress::MAX_APPEND_ATTEMPTS {
        state.reload()?;
        if message_ordinal(&state.conversation()?, &request.message_id)?.is_some() {
            return Ok("recorded");
        }
        let expected = state.head().clone();
        let transition = Transition::MessageAppend {
            entry: entry(conversation, request, role),
            payloads: Vec::new(),
        };
        match state.try_append_at(&expected, transition)? {
            TryAppend::Appended(_) => return Ok("recorded"),
            TryAppend::HeadChanged(_) => continue,
        }
    }
    Err(format!(
        "conversation {conversation:?} kept changing while its {what} was recorded"
    ))
}
