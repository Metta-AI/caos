//! Chat v2: one append-only ref containing every durable conversation event.
//!
//! The only authoritative pointer is
//! `refs/caos/conversations/<id>/head`. An idle submit publishes a queued user
//! event with one compare-and-swap push, then derives the exact request from
//! that durable commit. A submit during an active run appends only an
//! interjection. `llm-step` advances the same ref while it runs, so a client may
//! disappear after submit returns without owning unfinished conversation
//! state.

use std::io::{IsTerminal, Read, Write};

use serde_json::{json, Value};

use super::{curry_object, prepare_request, request_compute, GitTransport, Transport, CAOS_REMOTE};

const CONVERSATION_PREFIX: &str = "refs/caos/conversations/";
const HEAD_SUFFIX: &str = "/head";
const EVENT_VERSION: u64 = 2;
const MAX_APPEND_ATTEMPTS: usize = 32;
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const AUTO_NAME_PREFIX: &str = "talk-";
const MERGE_REF_CANDIDATES: &[&str] = &["main", "master", "origin/main", "origin/master"];
const DEFAULT_SYSTEM: &str = "You are a coding agent operating on a git workspace. Use the \
    available tools for file access, builds, tests, and edits. Keep responses concise.";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnOptions {
    pub base: Option<String>,
    pub system: Option<String>,
    pub system_file: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub username: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationMessage {
    pub author: String,
    pub username: Option<String>,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationSnapshot {
    pub id: String,
    pub head: String,
    pub title: String,
    pub status: String,
    pub request: Option<String>,
    /// The queued user event from which the active request is derived. This is
    /// retained after the worker records `request`, so a follower can identify
    /// the turn without treating later interjections as its input head.
    pub request_head: Option<String>,
    pub messages: Vec<ConversationMessage>,
}

#[derive(Clone, Debug)]
struct StoredEvent {
    commit: String,
    value: Value,
}

/// Choose an explicit conversation, the newest existing one, or a fresh
/// `talk-N`. The remote canonical refs are the source of truth.
pub fn pick_conversation(
    t: &GitTransport,
    requested: Option<&str>,
    new: bool,
) -> Result<(String, bool), String> {
    if let Some(id) = requested {
        let refname = conversation_ref(id)?;
        let exists = remote_ref(t, &refname)?.is_some();
        if new && exists {
            return Err(format!(
                "--new: conversation {id:?} already exists (omit --new to continue it)"
            ));
        }
        return Ok((id.to_string(), !exists));
    }

    let mut conversations = remote_conversations(t)?;
    if !new && !conversations.is_empty() {
        // Read only each tip object so the choice is based on commit time rather
        // than lexicographic ref order. Fetching a tip through Git would also
        // fetch its entire workspace/history closure, which is especially
        // costly when opening a shared remote with many conversations.
        let mut dated = Vec::with_capacity(conversations.len());
        for (id, hash) in conversations.drain(..) {
            let timestamp = remote_commit_timestamp(t, &hash)?;
            dated.push((timestamp, id));
        }
        dated.sort_by(|a, b| b.cmp(a));
        return Ok((dated[0].1.clone(), false));
    }

    let used: std::collections::HashSet<String> =
        conversations.into_iter().map(|(id, _)| id).collect();
    for number in 1_u64.. {
        let id = format!("{AUTO_NAME_PREFIX}{number}");
        if !used.contains(&id) {
            return Ok((id, true));
        }
    }
    unreachable!("the integer conversation-name space is not finite")
}

/// Rebuild a conversation entirely from its remote first-parent event log.
/// The local ref is updated only as an expendable cache.
pub fn conversation_snapshot(
    t: &GitTransport,
    id: &str,
) -> Result<Option<ConversationSnapshot>, String> {
    let refname = conversation_ref(id)?;
    let Some(head) = remote_ref(t, &refname)? else {
        return Ok(None);
    };
    fetch_conversation_commit(t, &refname, &head)?;

    let snapshot = conversation_snapshot_at(t, id, &head)?;
    // This ref is only a fetch-negotiation cache. Once the remote head has
    // been read successfully, a local lock race or read-only checkout must not
    // turn that authoritative state into a failed refresh.
    let _ = update_local_cache(t, &refname, &head);
    Ok(Some(snapshot))
}

/// Read only the authoritative pointer. Followers use this cheap check before
/// rebuilding the transcript, which makes an unchanged poll independent of the
/// conversation's length.
pub fn conversation_head(t: &GitTransport, id: &str) -> Result<Option<String>, String> {
    remote_ref(t, &conversation_ref(id)?)
}

fn conversation_snapshot_at(
    t: &GitTransport,
    id: &str,
    head: &str,
) -> Result<ConversationSnapshot, String> {
    fetch_commit(t, head)?;

    let mut newest_first = Vec::new();
    let mut current = head.to_string();
    loop {
        let message = t.git_capture(&["show", "-s", "--format=%B", &current], None)?;
        let value = match serde_json::from_str::<Value>(message.trim()) {
            Ok(value)
                if value.is_object()
                    && value.get("v").and_then(Value::as_u64) == Some(EVENT_VERSION) =>
            {
                value
            }
            // The first non-event parent is the ordinary workspace commit on
            // which the conversation began. The canonical tip itself, however,
            // must always be an event: treating a corrupt or mispointed tip as
            // an empty conversation makes intact history appear to vanish.
            _ if !newest_first.is_empty() => break,
            _ => {
                return Err(format!(
                    "conversation head {head} is not a version-{EVENT_VERSION} event"
                ))
            }
        };
        newest_first.push(StoredEvent {
            commit: current.clone(),
            value,
        });
        let parent = t.git_capture(
            &["rev-parse", "--verify", "--quiet", &format!("{current}^")],
            None,
        );
        match parent {
            Ok(parent) => current = parent.trim().to_string(),
            Err(_) => break,
        }
    }
    newest_first.reverse();
    fold_events(id, head, &newest_first)
}

/// Persist a user message before returning. An idle submit returns the queued
/// event commit from which the caller should prepare the request; an active
/// submit appends only the message and returns `None`, because the active turn
/// owns request admission.
pub fn submit_message(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    message: &str,
) -> Result<Option<String>, String> {
    submit_message_inner(t, options, id, message, false)
}

/// Submit the first message for `--new`, failing instead of joining a
/// conversation that another client created under the same auto-name.
pub fn submit_new_message(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    message: &str,
) -> Result<Option<String>, String> {
    submit_message_inner(t, options, id, message, true)
}

fn submit_message_inner(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    message: &str,
    require_absent: bool,
) -> Result<Option<String>, String> {
    let refname = conversation_ref(id)?;
    if message.trim().is_empty() {
        return Err("empty message".to_string());
    }
    if options.system.is_some() && options.system_file.is_some() {
        return Err("--system and --system-file are mutually exclusive".to_string());
    }
    let username = resolve_username(t, options.username.as_deref())?;
    for _attempt in 0..MAX_APPEND_ATTEMPTS {
        let observed = remote_ref(t, &refname)?;
        if require_absent && observed.is_some() {
            return Err(format!(
                "--new: conversation {id:?} was created by another client; choose another name"
            ));
        }
        let parent = match observed.as_deref() {
            Some(head) => {
                fetch_commit(t, head)?;
                head.to_string()
            }
            None => resolve_base(t, options)?,
        };
        let tree = t
            .git_capture(&["rev-parse", &format!("{parent}^{{tree}}")], None)?
            .trim()
            .to_string();
        if observed.is_none()
            && t.git_capture(
                &["rev-parse", "--verify", "--quiet", &format!("{tree}:.caos")],
                None,
            )
            .is_ok()
        {
            return Err(
                "the base tree contains top-level .caos state; choose a clean base".to_string(),
            );
        }

        let mut user_event = json!({
            "v": EVENT_VERSION,
            "author": "user",
            "username": username,
            "content": message,
        });
        if observed.is_none() {
            user_event["title"] = Value::String(default_title(message));
        }

        if let Some(head) = observed.as_deref() {
            let snapshot = conversation_snapshot_at(t, id, head)?;
            if request_is_active(&snapshot.status) {
                let user = create_event_commit(t, &tree, &parent, &user_event)?;
                match push_head_cas(t, &refname, observed.as_deref(), &user)? {
                    true => {
                        // The remote CAS is the durability boundary. This local
                        // ref is merely a cache and may be locked by another TUI
                        // in the same checkout.
                        let _ = update_local_cache(t, &refname, &user);
                        return Ok(None);
                    }
                    false => continue,
                }
            }
        }

        user_event["status"] = Value::String("queued".to_string());
        let user = create_event_commit(t, &tree, &parent, &user_event)?;

        match push_head_cas(t, &refname, observed.as_deref(), &user)? {
            true => {
                // This push is the durability boundary. Return before request
                // preparation so callers cannot mistake a later launch error
                // for a failed append and submit the same message twice.
                let _ = update_local_cache(t, &refname, &user);
                return Ok(Some(user));
            }
            false => continue,
        }
    }
    Err(format!(
        "conversation {id:?} kept changing after {MAX_APPEND_ATTEMPTS} submit attempts"
    ))
}

/// Derive and publish the exact `llm-step` request for a queued user event.
/// Repeating this for the same event and options returns the same request hash;
/// `/run` supplies the ordinary CAOS join/cache/restart behavior.
pub fn prepare_queued_request(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    queued_head: &str,
) -> Result<String, String> {
    validate_hash(queued_head, "queued conversation head")?;
    let llm = resolve_llm(t, options, id)?;
    prepare_request(
        t,
        &llm,
        None,
        &[format!("--head:commit={queued_head}")],
        &[],
    )
}

/// Resolve the human-facing identity once per client. `author` remains
/// protocol-level `user`; this name is presentation metadata only.
pub fn resolve_username(t: &GitTransport, explicit: Option<&str>) -> Result<String, String> {
    if let Some(explicit) = explicit {
        return normalized_username(explicit)
            .ok_or_else(|| "--username must be nonempty and contain no control characters".into());
    }
    if let Ok(configured) = t.git_capture(&["config", "--get", "user.name"], None) {
        if let Some(configured) = normalized_username(&configured) {
            return Ok(configured);
        }
    }
    if let Some(user) = std::env::var_os("USER") {
        let user = user.to_string_lossy();
        if let Some(user) = normalized_username(&user) {
            return Ok(user);
        }
    }
    Ok("user".to_string())
}

fn normalized_username(username: &str) -> Option<String> {
    let username = username.trim();
    (!username.is_empty() && !username.chars().any(char::is_control)).then(|| username.to_string())
}

fn resolve_llm(t: &GitTransport, options: &TurnOptions, id: &str) -> Result<String, String> {
    let api_key = std::env::var(API_KEY_ENV)
        .map_err(|_| format!("{API_KEY_ENV} must be set to start a conversation request"))?;
    let system = match (&options.system, &options.system_file) {
        (Some(system), None) => system.clone(),
        (None, Some(path)) => std::fs::read_to_string(path)
            .map_err(|error| format!("reading --system-file {path}: {error}"))?,
        (None, None) => DEFAULT_SYSTEM.to_string(),
        (Some(_), Some(_)) => {
            return Err("--system and --system-file are mutually exclusive".into())
        }
    };
    let merge_refs = snapshot_merge_refs(t)?;
    let mut config = vec![
        format!("--api-key={api_key}"),
        format!("--system={system}"),
        format!("--conversation={id}"),
    ];
    if !merge_refs.is_empty() {
        config.push(format!("--merge-refs={merge_refs}"));
    }
    if let Some(model) = &options.model {
        config.push(format!("--model={model}"));
    }
    if let Some(base_url) = &options.base_url {
        config.push(format!("--base-url={base_url}"));
    }
    let llm_base = crate::eval::eval_workspace_dep(t, "llm-step")?;
    curry_object(t, &llm_base, None, &[], &config).map(|hash| hash.to_string())
}

fn request_is_active(status: &str) -> bool {
    matches!(status, "queued" | "running")
}

/// Join, cache-hit, or restart an already-recorded request. This owns no
/// conversation state; `llm-step` advances the canonical head itself.
pub fn resume_request(t: &GitTransport, request: &str) -> Result<(), String> {
    validate_hash(request, "request")?;
    let server = t.server_url()?;
    request_compute(&server, request, "").map(|_| ())
}

fn resolve_base(t: &GitTransport, options: &TurnOptions) -> Result<String, String> {
    let rev = options.base.as_deref().unwrap_or("HEAD");
    t.resolve_revspec(rev)?
        .map(|oid| oid.to_string())
        .ok_or_else(|| format!("cannot resolve conversation base {rev:?}"))
}

fn snapshot_merge_refs(t: &GitTransport) -> Result<String, String> {
    let mut lines = String::new();
    for spec in MERGE_REF_CANDIDATES {
        // These are optional conveniences for the merge tool. A repository
        // commonly has only one of them; an absent candidate is not a broken
        // conversation. Use quiet rev-parse directly so the required-base
        // resolver can retain its deliberately loud missing-revision error.
        let candidate = format!("{spec}^{{commit}}");
        let Ok(hash) = t.git_capture(&["rev-parse", "--verify", "--quiet", &candidate], None)
        else {
            continue;
        };
        let hash = hash.trim();
        validate_hash(hash, "merge ref")?;
        t.ensure_pushed(hash)?;
        lines.push_str(spec);
        lines.push(' ');
        lines.push_str(hash);
        lines.push('\n');
    }
    // Conversation heads are ordinary merge candidates too. This is what
    // makes a forked experiment usable from the original conversation without
    // inventing a chat-specific merge protocol: the model hands the selected
    // commit to the existing std/merge worker.
    for (id, hash) in remote_conversations(t)? {
        lines.push_str(&conversation_ref(&id)?);
        lines.push(' ');
        lines.push_str(&hash);
        lines.push('\n');
    }
    Ok(lines)
}

fn create_event_commit(
    t: &GitTransport,
    tree: &str,
    parent: &str,
    event: &Value,
) -> Result<String, String> {
    validate_hash(tree, "tree")?;
    validate_hash(parent, "parent")?;
    if !event.is_object() || event.get("v").and_then(Value::as_u64) != Some(EVENT_VERSION) {
        return Err("conversation event must be a version-2 JSON object".to_string());
    }
    let message = serde_json::to_string(event)
        .map_err(|error| format!("serializing conversation event: {error}"))?;
    let commit = t
        .git_capture(&["commit-tree", tree, "-p", parent, "-m", &message], None)?
        .trim()
        .to_string();
    validate_hash(&commit, "event commit")?;
    Ok(commit)
}

/// Push `candidate` only if the remote ref still has `expected` (or remains
/// absent). `false` means a clean CAS race; errors mean the same expected value
/// was observed after a failed push, so blindly retrying would hide a real
/// transport or server failure.
fn push_head_cas(
    t: &GitTransport,
    refname: &str,
    expected: Option<&str>,
    candidate: &str,
) -> Result<bool, String> {
    validate_hash(candidate, "candidate head")?;
    if let Some(expected) = expected {
        validate_hash(expected, "expected head")?;
    }
    let lease = match expected {
        Some(expected) => format!("--force-with-lease={refname}:{expected}"),
        None => format!("--force-with-lease={refname}:"),
    };
    let update = format!("{candidate}:{refname}");
    let pushed = t.git_capture(&["push", "--quiet", &lease, CAOS_REMOTE, &update], None);
    if pushed.is_ok() {
        return Ok(true);
    }
    let observed = remote_ref(t, refname)?;
    if observed.as_deref() == Some(candidate) {
        return Ok(true);
    }
    if let Some(observed) = observed.as_deref() {
        // The server may have accepted this update even when the client lost
        // the push response. A different writer can then advance the ref before
        // this confirmation read. The event is already durable when it is on
        // the authoritative first-parent spine; appending it again would
        // duplicate a submitted message.
        fetch_commit_after(t, observed, expected)?;
        if first_parent_contains(t, observed, candidate)? {
            return Ok(true);
        }
    }
    if observed.as_deref() != expected {
        return Ok(false);
    }
    Err(pushed.expect_err("checked error above"))
}

fn conversation_ref(id: &str) -> Result<String, String> {
    if id.is_empty() || id.split('/').any(|part| part == "head") {
        return Err(format!("invalid conversation id {id:?}"));
    }
    let refname = format!("{CONVERSATION_PREFIX}{id}{HEAD_SUFFIX}");
    let status = std::process::Command::new("git")
        .args(["check-ref-format", &refname])
        .status()
        .map_err(|error| format!("validating conversation id {id:?}: {error}"))?;
    if !status.success() {
        return Err(format!("invalid conversation id {id:?}"));
    }
    Ok(refname)
}

fn remote_ref(t: &GitTransport, refname: &str) -> Result<Option<String>, String> {
    let output = t.git_capture(&["ls-remote", "--refs", CAOS_REMOTE, refname], None)?;
    let mut lines = output.lines();
    let result = lines.next().and_then(|line| line.split_whitespace().next());
    if lines.next().is_some() {
        return Err(format!("server advertised {refname} more than once"));
    }
    result
        .map(|hash| {
            validate_hash(hash, "remote ref")?;
            Ok(hash.to_string())
        })
        .transpose()
}

fn remote_conversations(t: &GitTransport) -> Result<Vec<(String, String)>, String> {
    let pattern = format!("{CONVERSATION_PREFIX}*{HEAD_SUFFIX}");
    let output = t.git_capture(&["ls-remote", "--refs", CAOS_REMOTE, &pattern], None)?;
    let mut conversations = Vec::new();
    for line in output.lines() {
        let Some((hash, refname)) = line.split_once('\t') else {
            continue;
        };
        validate_hash(hash, "remote conversation head")?;
        let Some(id) = refname
            .strip_prefix(CONVERSATION_PREFIX)
            .and_then(|rest| rest.strip_suffix(HEAD_SUFFIX))
        else {
            continue;
        };
        conversation_ref(id)?;
        conversations.push((id.to_string(), hash.to_string()));
    }
    Ok(conversations)
}

fn remote_commit_timestamp(t: &GitTransport, hash: &str) -> Result<i64, String> {
    validate_hash(hash, "conversation tip")?;
    let mut current = hash.to_string();
    // Worker events deliberately use epoch-zero commit identities so recovery
    // recreates the same proposal. Follow raw commit objects (not their trees)
    // to the newest user event, whose ordinary client timestamp is the useful
    // measure of which conversation was most recently used.
    for _ in 0..4096 {
        let (kind, content) = t.get_object(&current)?;
        if kind != "commit" {
            return Err(format!(
                "conversation history object {current} is a {kind}, not a commit"
            ));
        }
        let text = std::str::from_utf8(&content).map_err(|error| {
            format!("conversation history commit {current} is not UTF-8: {error}")
        })?;
        let (headers, message) = text.split_once("\n\n").ok_or_else(|| {
            format!("conversation history commit {current} has no message separator")
        })?;
        let event = serde_json::from_str::<Value>(message.trim()).ok();
        if event
            .as_ref()
            .and_then(|event| event.get("author"))
            .and_then(Value::as_str)
            == Some("user")
        {
            let line = headers
                .lines()
                .find(|line| line.starts_with("committer "))
                .ok_or_else(|| format!("conversation event {current} has no committer header"))?;
            return line
                .split_whitespace()
                .rev()
                .nth(1)
                .ok_or_else(|| format!("conversation event {current} has no commit timestamp"))?
                .parse::<i64>()
                .map_err(|error| {
                    format!("conversation event {current} has an invalid timestamp: {error}")
                });
        }
        let Some(parent) = headers
            .lines()
            .find_map(|line| line.strip_prefix("parent "))
        else {
            return Err(format!(
                "conversation tip {hash} has no version-2 user event ancestor"
            ));
        };
        validate_hash(parent, "conversation event parent")?;
        current = parent.to_string();
    }
    Err(format!(
        "conversation history from {hash} exceeded 4096 commits before a user event"
    ))
}

fn fetch_commit(t: &GitTransport, hash: &str) -> Result<(), String> {
    validate_hash(hash, "commit")?;
    if t.git_capture(&["cat-file", "-e", &format!("{hash}^{{commit}}")], None)
        .is_ok()
    {
        return Ok(());
    }
    t.fetch_object(hash)
}

fn fetch_commit_after(
    t: &GitTransport,
    hash: &str,
    known_server_tip: Option<&str>,
) -> Result<(), String> {
    validate_hash(hash, "commit")?;
    if t.git_capture(&["cat-file", "-e", &format!("{hash}^{{commit}}")], None)
        .is_ok()
    {
        return Ok(());
    }
    if let Some(tip) = known_server_tip {
        validate_hash(tip, "negotiation tip")?;
        if t.git_capture(&["cat-file", "-e", &format!("{tip}^{{commit}}")], None)
            .is_ok()
        {
            return t
                .git_capture(
                    &[
                        "-c",
                        "fetch.negotiationAlgorithm=default",
                        "fetch",
                        "--quiet",
                        "--no-write-fetch-head",
                        "--negotiation-tip",
                        tip,
                        CAOS_REMOTE,
                        hash,
                    ],
                    None,
                )
                .map(|_| ())
                .map_err(|error| {
                    format!("fetching {hash} from {CAOS_REMOTE} after {tip}: {error}")
                });
        }
    }
    t.fetch_object(hash)
}

fn fetch_conversation_commit(t: &GitTransport, refname: &str, hash: &str) -> Result<(), String> {
    let cached = t
        .git_capture(&["rev-parse", "--verify", "--quiet", refname], None)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| validate_hash(value, "cached conversation head").is_ok());
    // A tip-only HTTP read (used while choosing the newest conversation) puts
    // raw commits in the local ODB without their trees. A private cache ref is
    // our marker that a closure fetch completed, but still verify the tip tree
    // before trusting it in case an older client wrote that ref prematurely.
    if cached.as_deref() == Some(hash) && commit_tree_is_local(t, hash) {
        return Ok(());
    }
    if let Some(tip) = cached.as_deref() {
        if commit_tree_is_local(t, tip) {
            return t
                .git_capture(
                    &[
                        "-c",
                        "fetch.negotiationAlgorithm=default",
                        "fetch",
                        "--quiet",
                        "--no-write-fetch-head",
                        "--negotiation-tip",
                        tip,
                        CAOS_REMOTE,
                        hash,
                    ],
                    None,
                )
                .map(|_| ())
                .map_err(|error| {
                    format!("fetching {hash} from {CAOS_REMOTE} after {tip}: {error}")
                });
        }
    }
    t.fetch_object(hash)
}

fn commit_tree_is_local(t: &GitTransport, hash: &str) -> bool {
    t.git_capture(&["cat-file", "-e", &format!("{hash}^{{tree}}")], None)
        .is_ok()
}

fn first_parent_contains(t: &GitTransport, tip: &str, ancestor: &str) -> Result<bool, String> {
    validate_hash(tip, "first-parent tip")?;
    validate_hash(ancestor, "first-parent ancestor")?;
    if tip == ancestor {
        return Ok(true);
    }
    let history = t.git_capture(&["rev-list", "--first-parent", tip], None)?;
    Ok(history.lines().any(|commit| commit == ancestor))
}

fn update_local_cache(t: &GitTransport, refname: &str, hash: &str) -> Result<(), String> {
    t.git_capture(&["update-ref", refname, hash], None)
        .map(|_| ())
}

fn validate_hash(hash: &str, what: &str) -> Result<(), String> {
    if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("invalid {what} hash {hash:?}"));
    }
    Ok(())
}

fn fold_events(
    id: &str,
    head: &str,
    events: &[StoredEvent],
) -> Result<ConversationSnapshot, String> {
    let mut messages = Vec::new();
    let mut title: Option<String> = None;
    let mut status: Option<String> = None;
    let mut request: Option<String> = None;
    let mut request_head: Option<String> = None;
    for event in events {
        let value = &event.value;
        if let Some(content) = value.get("content") {
            let content = content
                .as_str()
                .ok_or_else(|| "conversation event content is not a string".to_string())?;
            let author = value
                .get("author")
                .and_then(Value::as_str)
                .ok_or_else(|| "conversation event content has no string author".to_string())?;
            let username = match value.get("username") {
                Some(Value::String(username)) => Some(username.clone()),
                Some(Value::Null) | None => None,
                Some(_) => {
                    return Err("conversation event username is not a string or null".to_string())
                }
            };
            // Tool-call events can carry an empty textual projection. They are
            // durable protocol records, but not visible chat messages.
            if !content.is_empty() {
                messages.push(ConversationMessage {
                    author: author.to_string(),
                    username,
                    content: content.to_string(),
                });
            }
        }
        waterfall_string(value, "title", &mut title)?;
        waterfall_string(value, "status", &mut status)?;
        waterfall_string(value, "request", &mut request)?;
        match value.get("status").and_then(Value::as_str) {
            Some("queued") => request_head = Some(event.commit.clone()),
            Some("idle" | "failed" | "canceled") => request_head = None,
            _ => {}
        }
    }
    let title = title
        .or_else(|| {
            messages
                .first()
                .map(|message| default_title(&message.content))
        })
        .unwrap_or_else(|| id.to_string());
    Ok(ConversationSnapshot {
        id: id.to_string(),
        head: head.to_string(),
        title,
        status: status.unwrap_or_else(|| "idle".to_string()),
        request,
        request_head,
        messages,
    })
}

fn waterfall_string(value: &Value, key: &str, target: &mut Option<String>) -> Result<(), String> {
    let Some(next) = value.get(key) else {
        return Ok(());
    };
    if next.is_null() {
        *target = None;
        return Ok(());
    }
    let next = next
        .as_str()
        .ok_or_else(|| format!("conversation event {key} is not a string or null"))?;
    *target = Some(next.to_string());
    Ok(())
}

fn default_title(message: &str) -> String {
    const MAX_CHARS: usize = 60;
    let compact = message.split_whitespace().collect::<Vec<_>>().join(" ");
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

// ---------------------------------------------------------------------------
// Small line-oriented clients. They use the same durable submit API as the TUI
// and block only as a presentation convenience.
// ---------------------------------------------------------------------------

pub fn cli_chat(t: &GitTransport, args: &[String]) -> Result<(), String> {
    let Some(id) = args.first().filter(|arg| !arg.starts_with('-')) else {
        return Err("usage: caos chat <conversation> [-m <message>] [options]".to_string());
    };
    let parsed = parse_cli_args(&args[1..], false)?;
    run_line_client(t, Some(id), parsed)
}

pub fn cli_talk(t: &GitTransport, args: &[String]) -> Result<(), String> {
    let parsed = parse_cli_args(args, true)?;
    let name = parsed.name.clone();
    run_line_client(t, name.as_deref(), parsed)
}

#[derive(Default)]
struct LineArgs {
    name: Option<String>,
    new: bool,
    log: bool,
    message: Option<String>,
    options: TurnOptions,
}

fn parse_cli_args(args: &[String], positional_message: bool) -> Result<LineArgs, String> {
    let mut parsed = LineArgs::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let next = |index: &mut usize| -> Result<String, String> {
            *index += 1;
            args.get(*index)
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "-c" | "--conversation" if positional_message => parsed.name = Some(next(&mut index)?),
            "-m" | "--message" => parsed.message = Some(next(&mut index)?),
            "--new" if positional_message => parsed.new = true,
            "--log" => parsed.log = true,
            "--base" => parsed.options.base = Some(next(&mut index)?),
            "--system" => parsed.options.system = Some(next(&mut index)?),
            "--system-file" => parsed.options.system_file = Some(next(&mut index)?),
            "--model" => parsed.options.model = Some(next(&mut index)?),
            "--base-url" => parsed.options.base_url = Some(next(&mut index)?),
            "--username" => parsed.options.username = Some(next(&mut index)?),
            "-h" | "--help" => return Err("see `caos --help`".to_string()),
            other if positional_message && !other.starts_with('-') => {
                if parsed.message.is_some() {
                    return Err(
                        "talk accepts one prompt; quote prompts containing spaces".to_string()
                    );
                }
                parsed.message = Some(other.to_string());
            }
            other => return Err(format!("unknown chat option {other:?}")),
        }
        index += 1;
    }
    Ok(parsed)
}

fn run_line_client(
    t: &GitTransport,
    explicit_name: Option<&str>,
    parsed: LineArgs,
) -> Result<(), String> {
    let (id, fresh) = pick_conversation(t, explicit_name, parsed.new)?;
    if parsed.log {
        let snapshot =
            conversation_snapshot(t, &id)?.ok_or_else(|| format!("no conversation {id:?}"))?;
        print_snapshot(&snapshot, &mut std::io::stdout())?;
        return Ok(());
    }
    eprintln!("[conversation {id}{}]", if fresh { " — new" } else { "" });
    let require_absent = parsed.new && fresh;

    if let Some(message) = parsed.message {
        run_line_turn(t, &parsed.options, &id, &message, require_absent)?;
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        let mut message = String::new();
        std::io::stdin()
            .read_to_string(&mut message)
            .map_err(|error| format!("reading message: {error}"))?;
        return run_line_turn(t, &parsed.options, &id, &message, require_absent);
    }

    let mut line = String::new();
    let mut require_absent = require_absent;
    loop {
        eprint!("> ");
        std::io::stderr()
            .flush()
            .map_err(|error| error.to_string())?;
        line.clear();
        if std::io::stdin()
            .read_line(&mut line)
            .map_err(|error| format!("reading message: {error}"))?
            == 0
        {
            return Ok(());
        }
        if !line.trim().is_empty() {
            run_line_turn(t, &parsed.options, &id, &line, require_absent)?;
            require_absent = false;
        }
    }
}

fn run_line_turn(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    message: &str,
    require_absent: bool,
) -> Result<(), String> {
    let before = conversation_snapshot(t, id)?
        .map(|snapshot| snapshot.messages.len())
        .unwrap_or_default();
    let submitted = if require_absent {
        submit_new_message(t, options, id, message)?
    } else {
        submit_message(t, options, id, message)?
    };
    if let Some(queued_head) = submitted {
        let request = prepare_queued_request(t, options, id, &queued_head)?;
        resume_request(t, &request)?;
    }
    let snapshot = conversation_snapshot(t, id)?
        .ok_or_else(|| format!("conversation {id:?} disappeared after its request"))?;
    for message in snapshot.messages.iter().skip(before) {
        if message.author == "assistant" || message.author == "agent" {
            println!("{}", message.content);
        }
    }
    Ok(())
}

fn print_snapshot(snapshot: &ConversationSnapshot, output: &mut impl Write) -> Result<(), String> {
    for message in &snapshot.messages {
        let author = if matches!(message.author.as_str(), "user" | "human") {
            message.username.as_deref().unwrap_or(&message.author)
        } else {
            &message.author
        };
        writeln!(output, "{}: {}", author, message.content)
            .map_err(|error| format!("printing conversation: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_git(dir: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn configure_test_repo(repo: &std::path::Path, username: &str) {
        test_git(repo, &["config", "user.name", username]);
        test_git(repo, &["config", "user.email", "test@example.com"]);
    }

    fn event(value: Value) -> StoredEvent {
        event_at("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", value)
    }

    fn event_at(commit: &str, value: Value) -> StoredEvent {
        StoredEvent {
            commit: commit.to_string(),
            value,
        }
    }

    #[test]
    fn event_fold_builds_transcript_and_waterfall_state() {
        let snapshot = fold_events(
            "one",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &[
                event(json!({"v": 2, "author": "user", "username": "Alice", "content": "hello", "status": "queued", "title": "First"})),
                event(json!({"v": 2, "request": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "status": "running"})),
                event(json!({"v": 2, "author": "assistant", "content": "", "calls": [{"name": "bash"}]})),
                event(json!({"v": 2, "author": "assistant", "content": "done", "status": "idle"})),
            ],
        )
        .unwrap();
        assert_eq!(snapshot.title, "First");
        assert_eq!(snapshot.status, "idle");
        assert_eq!(snapshot.request_head, None);
        assert_eq!(snapshot.messages.len(), 2);
        assert_eq!(snapshot.messages[0].author, "user");
        assert_eq!(snapshot.messages[0].username.as_deref(), Some("Alice"));
        assert_eq!(snapshot.messages[1].content, "done");
        assert_eq!(
            snapshot.request.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    #[test]
    fn null_clears_waterfall_values() {
        let snapshot = fold_events(
            "fallback",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &[
                event(json!({"v": 2, "title": "old", "request": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"})),
                event(json!({"v": 2, "title": null, "request": null})),
            ],
        )
        .unwrap();
        assert_eq!(snapshot.title, "fallback");
        assert_eq!(snapshot.request, None);
    }

    #[test]
    fn titles_are_compact_and_bounded() {
        assert_eq!(default_title("  one\n two  "), "one two");
        assert_eq!(default_title(&"x".repeat(80)).chars().count(), 60);
        assert!(default_title(&"x".repeat(80)).ends_with('…'));
    }

    #[test]
    fn hash_validation_is_exact() {
        assert!(validate_hash(&"a".repeat(40), "test").is_ok());
        assert!(validate_hash(&"a".repeat(39), "test").is_err());
        assert!(validate_hash(&"z".repeat(40), "test").is_err());
    }

    #[test]
    fn active_states_turn_a_concurrent_message_into_an_interjection() {
        assert!(request_is_active("queued"));
        assert!(request_is_active("running"));
        assert!(!request_is_active("idle"));
        assert!(!request_is_active("failed"));
    }

    #[test]
    fn queued_head_survives_interjections_and_request_recording() {
        let queued = "1111111111111111111111111111111111111111";
        let snapshot = fold_events(
            "one",
            "4444444444444444444444444444444444444444",
            &[
                event_at(
                    queued,
                    json!({"v": 2, "author": "user", "content": "start", "status": "queued"}),
                ),
                event_at(
                    "2222222222222222222222222222222222222222",
                    json!({"v": 2, "author": "user", "content": "also this"}),
                ),
                event_at(
                    "3333333333333333333333333333333333333333",
                    json!({"v": 2, "request": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "status": "running"}),
                ),
            ],
        )
        .unwrap();

        assert_eq!(snapshot.request_head.as_deref(), Some(queued));
        assert_eq!(
            snapshot.request.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    #[test]
    fn invalid_username_metadata_fails_loudly() {
        let error = fold_events(
            "one",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &[event(
                json!({"v": 2, "author": "user", "username": 7, "content": "hello"}),
            )],
        )
        .unwrap_err();
        assert!(error.contains("username"));
    }

    #[test]
    fn malformed_canonical_head_fails_instead_of_hiding_history() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = std::env::temp_dir().join(format!(
            "caos-chat-v2-malformed-head-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&repo).unwrap();
        test_git(&repo, &["init", "--quiet"]);
        configure_test_repo(&repo, "test");
        std::fs::write(repo.join("workspace"), "base\n").unwrap();
        test_git(&repo, &["add", "workspace"]);
        test_git(&repo, &["commit", "--quiet", "-m", "base"]);
        let base = test_git(&repo, &["rev-parse", "HEAD"]);
        let tree = test_git(&repo, &["rev-parse", "HEAD^{tree}"]);
        let malformed = test_git(
            &repo,
            &["commit-tree", &tree, "-p", &base, "-m", "not an event"],
        );

        let transport = GitTransport::discover(&repo).unwrap();
        let error = conversation_snapshot_at(&transport, "broken", &malformed).unwrap_err();
        assert!(error.contains("conversation head"));
        assert!(error.contains("version-2 event"));

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn line_clients_parse_explicit_usernames() {
        let parsed = parse_cli_args(
            &[
                "--username".to_string(),
                "Alice Smith".to_string(),
                "hello".to_string(),
            ],
            true,
        )
        .unwrap();
        assert_eq!(parsed.options.username.as_deref(), Some("Alice Smith"));
        assert_eq!(parsed.message.as_deref(), Some("hello"));
        assert_eq!(
            normalized_username("  Alice Smith  ").as_deref(),
            Some("Alice Smith")
        );
        assert!(normalized_username("Alice\nBob").is_none());
    }

    #[test]
    fn concurrent_clients_preserve_both_named_interjections() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "caos-chat-v2-multiplayer-{}-{unique}",
            std::process::id()
        ));
        let remote = root.join("remote.git");
        let seed = root.join("seed");
        let alice = root.join("alice");
        let bob = root.join("bob");
        std::fs::create_dir_all(&remote).unwrap();
        test_git(&remote, &["init", "--quiet", "--bare"]);
        std::fs::create_dir_all(&seed).unwrap();
        test_git(&seed, &["init", "--quiet"]);
        configure_test_repo(&seed, "seed");
        std::fs::write(seed.join("workspace"), "base\n").unwrap();
        test_git(&seed, &["add", "workspace"]);
        test_git(&seed, &["commit", "--quiet", "-m", "base"]);
        test_git(
            &seed,
            &["remote", "add", CAOS_REMOTE, remote.to_str().unwrap()],
        );

        let seed_transport = GitTransport::discover(&seed).unwrap();
        let base = test_git(&seed, &["rev-parse", "HEAD"]);
        let tree = test_git(&seed, &["rev-parse", "HEAD^{tree}"]);
        let first = create_event_commit(
            &seed_transport,
            &tree,
            &base,
            &json!({
                "v": 2,
                "author": "user",
                "username": "seed",
                "content": "start",
                "status": "queued"
            }),
        )
        .unwrap();
        let conversation_ref = conversation_ref("shared").unwrap();
        test_git(
            &seed,
            &[
                "push",
                "--quiet",
                CAOS_REMOTE,
                &format!("{first}:{conversation_ref}"),
            ],
        );

        test_git(
            &root,
            &["clone", "--quiet", seed.to_str().unwrap(), "alice"],
        );
        test_git(&root, &["clone", "--quiet", seed.to_str().unwrap(), "bob"]);
        for (repo, username) in [(&alice, "Alice"), (&bob, "Bob")] {
            configure_test_repo(repo, username);
            test_git(
                repo,
                &["remote", "add", CAOS_REMOTE, remote.to_str().unwrap()],
            );
        }

        // The canonical push must remain successful even when another local
        // process holds the expendable cache ref's lock.
        let alice_cache = alice.join(".git/refs/caos/conversations/shared");
        std::fs::create_dir_all(&alice_cache).unwrap();
        std::fs::write(alice_cache.join("head.lock"), "held by another TUI\n").unwrap();

        let alice_thread = std::thread::spawn({
            let alice = alice.clone();
            move || {
                let transport = GitTransport::discover(alice).unwrap();
                submit_message(
                    &transport,
                    &TurnOptions::default(),
                    "shared",
                    "  from Alice\n",
                )
                .unwrap()
            }
        });
        let bob_thread = std::thread::spawn({
            let bob = bob.clone();
            move || {
                let transport = GitTransport::discover(bob).unwrap();
                submit_message(&transport, &TurnOptions::default(), "shared", "from Bob").unwrap()
            }
        });
        let alice_result = alice_thread.join().unwrap();
        let bob_result = bob_thread.join().unwrap();
        assert_eq!(alice_result, None);
        assert_eq!(bob_result, None);

        let snapshot = conversation_snapshot(&GitTransport::discover(&alice).unwrap(), "shared")
            .unwrap()
            .unwrap();
        let named_messages: std::collections::HashMap<_, _> = snapshot
            .messages
            .iter()
            .filter_map(|message| {
                message
                    .username
                    .as_deref()
                    .map(|username| (username, message.content.as_str()))
            })
            .collect();
        assert_eq!(named_messages.get("Alice"), Some(&"  from Alice\n"));
        assert_eq!(named_messages.get("Bob"), Some(&"from Bob"));
        assert_eq!(snapshot.status, "queued");
        assert_eq!(snapshot.request, None);
        assert_eq!(snapshot.request_head.as_deref(), Some(first.as_str()));

        // A lost push response followed by another accepted event must be
        // classified as success, rather than replaying the ancestor event.
        let accepted_ancestor = test_git(&alice, &["rev-parse", &format!("{}^", snapshot.head)]);
        let alice_transport = GitTransport::discover(&alice).unwrap();
        assert!(push_head_cas(
            &alice_transport,
            &conversation_ref,
            Some(&first),
            &accepted_ancestor,
        )
        .unwrap());
        assert_eq!(
            remote_ref(&alice_transport, &conversation_ref)
                .unwrap()
                .as_deref(),
            Some(snapshot.head.as_str())
        );

        // Two `--new` clients may choose the same free talk-N. The loser must
        // fail visibly instead of silently joining the winner's request.
        let error = submit_new_message(
            &alice_transport,
            &TurnOptions::default(),
            "shared",
            "must not become an interjection",
        )
        .unwrap_err();
        assert!(error.contains("--new"));
        assert_eq!(
            remote_ref(&alice_transport, &conversation_ref)
                .unwrap()
                .as_deref(),
            Some(snapshot.head.as_str())
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
