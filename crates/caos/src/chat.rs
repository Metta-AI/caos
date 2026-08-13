//! Chat v2: one append-only ref containing every durable conversation event.
//!
//! The only authoritative pointer is
//! `refs/caos/conversations/<id>/head`. A submit constructs the user event and
//! the exact request event locally, then publishes both with one compare-and-
//! swap push before execution starts. `llm-step` advances the same ref while it
//! runs. A client may disappear at any point after submit returns without
//! owning any unfinished conversation state.

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationMessage {
    pub author: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationSnapshot {
    pub id: String,
    pub head: String,
    pub title: String,
    pub status: String,
    pub request: Option<String>,
    pub messages: Vec<ConversationMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmittedTurn {
    pub request: String,
}

#[derive(Clone, Debug)]
struct StoredEvent {
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
        // Fetch each small tip so the choice is based on its commit time rather
        // than lexicographic ref order. This is presentation only; identity is
        // always the ref name.
        let mut dated = Vec::with_capacity(conversations.len());
        for (id, hash) in conversations.drain(..) {
            fetch_commit(t, &hash)?;
            let timestamp = t
                .git_capture(&["show", "-s", "--format=%ct", &hash], None)?
                .trim()
                .parse::<i64>()
                .unwrap_or_default();
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
    fetch_commit(t, &head)?;
    update_local_cache(t, &refname, &head)?;

    let mut newest_first = Vec::new();
    let mut current = head.clone();
    loop {
        let message = t.git_capture(&["show", "-s", "--format=%B", &current], None)?;
        let Ok(value) = serde_json::from_str::<Value>(message.trim()) else {
            break;
        };
        if value.get("v").and_then(Value::as_u64) != Some(EVENT_VERSION) {
            break;
        }
        if !value.is_object() {
            return Err(format!("conversation event {current} is not a JSON object"));
        }
        newest_first.push(StoredEvent { value });
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
    Ok(Some(fold_events(id, &head, &newest_first)?))
}

/// Persist a user message and its exact run identity before returning. The two
/// event commits are published in one CAS update, so there is no accepted user
/// event whose hidden request configuration was lost with its client.
pub fn submit_message(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    message: &str,
) -> Result<SubmittedTurn, String> {
    let refname = conversation_ref(id)?;
    let message = message.trim();
    if message.is_empty() {
        return Err("empty message".to_string());
    }
    if options.system.is_some() && options.system_file.is_some() {
        return Err("--system and --system-file are mutually exclusive".to_string());
    }

    let api_key = std::env::var(API_KEY_ENV)
        .map_err(|_| format!("{API_KEY_ENV} must be set to start a conversation request"))?;
    let system = match (&options.system, &options.system_file) {
        (Some(system), None) => system.clone(),
        (None, Some(path)) => std::fs::read_to_string(path)
            .map_err(|error| format!("reading --system-file {path}: {error}"))?,
        (None, None) => DEFAULT_SYSTEM.to_string(),
        (Some(_), Some(_)) => unreachable!("checked above"),
    };

    // Resolve the worker and all configuration before the durable submit. The
    // only per-attempt input still missing is the candidate user-event commit.
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
    let llm = curry_object(t, &llm_base, None, &[], &config)?.to_string();

    for _attempt in 0..MAX_APPEND_ATTEMPTS {
        let observed = remote_ref(t, &refname)?;
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
            "content": message,
            "status": "queued",
        });
        if observed.is_none() {
            user_event["title"] = Value::String(default_title(message));
        }
        let user = create_event_commit(t, &tree, &parent, &user_event)?;

        // `prepare_request` stores and pushes the whole exact ArgTree. Its head
        // argument points at the user event, not the request event, avoiding a
        // circular hash while keeping replay input explicit.
        let request = prepare_request(t, &llm, None, &[format!("--head:commit={user}")], &[])?;
        let request_event = json!({
            "v": EVENT_VERSION,
            "request": request,
            "status": "running",
        });
        let submitted = create_event_commit(t, &tree, &user, &request_event)?;

        match push_head_cas(t, &refname, observed.as_deref(), &submitted)? {
            true => {
                update_local_cache(t, &refname, &submitted)?;
                return Ok(SubmittedTurn { request });
            }
            false => continue,
        }
    }
    Err(format!(
        "conversation {id:?} kept changing after {MAX_APPEND_ATTEMPTS} submit attempts"
    ))
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
        if let Some(hash) = t.resolve_revspec(spec)? {
            let hash = hash.to_string();
            t.ensure_pushed(&hash)?;
            lines.push_str(spec);
            lines.push(' ');
            lines.push_str(&hash);
            lines.push('\n');
        }
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

fn fetch_commit(t: &GitTransport, hash: &str) -> Result<(), String> {
    validate_hash(hash, "commit")?;
    if t.git_capture(&["cat-file", "-e", &format!("{hash}^{{commit}}")], None)
        .is_ok()
    {
        return Ok(());
    }
    t.fetch_object(hash)
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
            messages.push(ConversationMessage {
                author: author.to_string(),
                content: content.to_string(),
            });
        }
        waterfall_string(value, "title", &mut title)?;
        waterfall_string(value, "status", &mut status)?;
        waterfall_string(value, "request", &mut request)?;
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

    if let Some(message) = parsed.message {
        run_line_turn(t, &parsed.options, &id, &message)?;
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        let mut message = String::new();
        std::io::stdin()
            .read_to_string(&mut message)
            .map_err(|error| format!("reading message: {error}"))?;
        return run_line_turn(t, &parsed.options, &id, &message);
    }

    let mut line = String::new();
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
            run_line_turn(t, &parsed.options, &id, &line)?;
        }
    }
}

fn run_line_turn(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    message: &str,
) -> Result<(), String> {
    let before = conversation_snapshot(t, id)?
        .map(|snapshot| snapshot.messages.len())
        .unwrap_or_default();
    let submitted = submit_message(t, options, id, message)?;
    resume_request(t, &submitted.request)?;
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
        writeln!(output, "{}: {}", message.author, message.content)
            .map_err(|error| format!("printing conversation: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(value: Value) -> StoredEvent {
        StoredEvent { value }
    }

    #[test]
    fn event_fold_builds_transcript_and_waterfall_state() {
        let snapshot = fold_events(
            "one",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &[
                event(json!({"v": 2, "author": "user", "content": "hello", "status": "queued", "title": "First"})),
                event(json!({"v": 2, "request": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "status": "running"})),
                event(json!({"v": 2, "author": "assistant", "content": "done", "status": "idle"})),
            ],
        )
        .unwrap();
        assert_eq!(snapshot.title, "First");
        assert_eq!(snapshot.status, "idle");
        assert_eq!(snapshot.messages.len(), 2);
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
}
