//! `caos-cli talk` / `chat` — the user-facing conversation client (see
//! design/agent-harness.md, "Client").
//!
//! One turn: mint the human-turn commit (parent = the conversation head, or
//! the base for a new conversation; tree = the parent's tree — human turns
//! are text-only for now), hand it to an `llm-step` run, watch the turn's
//! progress ref while the run blocks, and on success advance
//! `refs/caos/conversations/<name>/from-user` to the returned turn commit.
//! Conversation identity is that ref — the only mutable thing, owned by this
//! client. On a failed run the ref is untouched; the minted human commit is
//! harmlessly orphaned.
//!
//! `talk` is the everyday surface: the positional argument is the prompt, the
//! conversation defaults to the repo's most recently used one (`--new` starts
//! another), and with no prompt on a terminal it loops, one turn per line.
//! `chat <name>` is the explicit, scriptable form of the same turn.
//!
//! The workers run as `curry(runner, bin=<static binary>)` on the shared
//! runner pool. By default both come ready-made from the published library
//! (`/cas/std/bash-tool`, `/cas/std/llm-step` — see build-builtins.sh), so
//! there is nothing to build or commit locally; `--llm-step-bin` /
//! `--bash-tool-bin` (or the env vars) override with a local, git-tracked
//! binary — the stub tests' path.

use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Read};

use serde_json::Value;

use super::{
    curry_object, entry_name, fetch_blob_string, fetch_tree_entries, prepare_request,
    request_compute, resolve_cli_image, GitTransport, HttpTransport, Transport,
};

/// Author name on agent step/turn commits (see design/agent-harness.md): the
/// marker the conversation walk keys on, and therefore *reserved* — a human
/// turn must carry any other author.
const AGENT_AUTHOR: &str = "caos-agent";

/// The conversation namespace. Every ref of a conversation lives together
/// under `<prefix><id>/<channel>` (see design/talk-while-thinking.md, "Refs"):
///
/// * `from-user` — the user branch tip: the engine's local HEAD *and* the
///   server's canonical HEAD (now identically named), advancing to the turn
///   merge on completion. Client is its sole writer.
/// * `from-agent` — the agent's step-chain tip, pushed by the worker.
/// * `title` — the display-name blob (client).
/// * `status` — the worker's in-round status blob `"<human hash>\n<text>"`
///   (calling / retrying / answered-in; the hash scopes it to a turn, so a
///   stale one is ignorable).
///
/// The channel names are reserved in [`validated_refname`], so a conversation
/// id can't shadow another's channel.
const CONV_REF_PREFIX: &str = "refs/caos/conversations/";
const FROM_USER_CHANNEL: &str = "from-user";
const FROM_AGENT_CHANNEL: &str = "from-agent";
const TITLE_CHANNEL: &str = "title";
const STATUS_CHANNEL: &str = "status";
const RESERVED_CHANNELS: [&str; 4] = [
    FROM_USER_CHANNEL,
    FROM_AGENT_CHANNEL,
    TITLE_CHANNEL,
    STATUS_CHANNEL,
];

/// The LLM API key rides in from the environment, never a flag (it would land
/// in shell history and process listings).
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// Env fallbacks for the worker-binary paths.
const LLM_STEP_BIN_ENV: &str = "CAOS_LLM_STEP_BIN";
const BASH_TOOL_BIN_ENV: &str = "CAOS_BASH_TOOL_BIN";
const RGREP_BIN_ENV: &str = "CAOS_RGREP_BIN";

/// The std builtin the worker binaries run under (`curry(runner, bin=...)`),
/// used when a `--*-bin` override supplies the binary.
const RUNNER_IMAGE: &str = "/cas/std/runner";

/// The std-published, ready-to-run worker curries (build-builtins.sh) — the
/// defaults when no `--*-bin` override is given.
const BASH_TOOL_IMAGE: &str = "/cas/std/bash-tool";
const LLM_STEP_IMAGE: &str = "/cas/std/llm-step";
const RGREP_IMAGE: &str = "/cas/std/rgrep";
/// The script-worker image TREE TOOLS run on (the workspace's caos-tools/*.sh,
/// discovered per round, resolved at invocation time — design/cargo-workers.md).
/// Optional: a stack whose std predates it just doesn't register tree tools.
const TOOLS_IMAGE: &str = "/cas/std/bash";

/// The git-bearing merge worker (SPEC "Merging and conflict resolution").
/// Optional: a stack whose std predates it just doesn't offer the merge tool.
const MERGE_IMAGE: &str = "/cas/std/merge";

/// Refs snapshotted to hashes at turn start so the `merge` tool can resolve
/// `--theirs=<name>` (SPEC "Resolving `--theirs`"). Curated to the names a
/// model actually types; a spec that doesn't resolve is simply skipped.
const MERGE_REF_CANDIDATES: &[&str] = &["main", "master", "origin/main", "origin/master"];

/// Auto-named conversations (`talk` with no `-c`): `talk-1`, `talk-2`, …
const AUTO_NAME_PREFIX: &str = "talk-";

/// Default system prompt when neither `--system` nor `--system-file` is given.
const DEFAULT_SYSTEM: &str = "You are a coding agent operating on a git workspace. Use the \
     read/ls/write/edit tools for file access and grep to search. The workspace may define \
     its own tools (caos-tools/*.sh, e.g. build/test) — prefer them (cached caos jobs) over \
     doing the same work via bash, and know that editing a tool script changes what the tool \
     does on your next call. Use the bash tool to run other commands (scripts, generators), \
     declaring every path a command reads in `paths`. Keep responses concise.";

/// Milliseconds between progress/status polls while the run blocks. Each poll
/// is two `ls-remote`s plus a few object reads — cheap enough to keep short
/// turns feeling live.
const POLL_MS: u64 = 500;

/// Configuration for one agent turn. This is the presentation-independent
/// surface shared by the line-oriented CLI and richer clients such as the TUI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnOptions {
    pub base: Option<String>,
    pub system: Option<String>,
    pub system_file: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub llm_step_bin: Option<String>,
    pub bash_tool_bin: Option<String>,
    pub rgrep_bin: Option<String>,
}

/// One project-defined tool available to the selected conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDescription {
    pub name: String,
    pub docs: String,
    pub image: String,
}

/// Project-defined tools for a turn. The harness's built-ins are separate and
/// are identified as such by clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSetDescription {
    pub source: String,
    pub tools: Vec<ToolDescription>,
}

/// Describe the project tools visible to a conversation's current workspace.
/// Existing conversations use their virtual head; new conversations use their
/// configured base (or `HEAD`). Only the `caos-tools/*.sh` blobs are read.
pub fn describe_tool_set(
    t: &GitTransport,
    conversation: &str,
    options: &TurnOptions,
) -> Result<ToolSetDescription, String> {
    use gix::objs::tree::EntryKind;

    let conversation_ref = conversation_head_ref(conversation);
    let root = if rev_parse_opt(t, &conversation_ref)?.is_some() {
        conversation_ref
    } else {
        options.base.clone().unwrap_or_else(|| "HEAD".to_string())
    };
    let source = format!("{root}:caos-tools");
    let Some(tree) = rev_parse_opt(t, &source)? else {
        return Ok(ToolSetDescription {
            source,
            tools: Vec::new(),
        });
    };
    let entries = fetch_tree_entries(t, &tree)?
        .ok_or_else(|| format!("project tools object {tree} is not a tree"))?;
    let mut tools = Vec::new();
    for entry in entries {
        let filename = String::from_utf8(entry_name(&entry).to_vec())
            .map_err(|_| "tool name is not UTF-8".to_string())?;
        let Some(name) = filename.strip_suffix(".sh") else {
            continue;
        };
        if !matches!(
            entry.mode.kind(),
            EntryKind::Blob | EntryKind::BlobExecutable
        ) || ["bash", "grep", "read", "ls", "write", "edit"].contains(&name)
        {
            continue;
        }
        let script = fetch_blob_string(t, &entry.oid.to_string())?;
        let docs: Vec<&str> = script
            .lines()
            .filter_map(|line| line.strip_prefix("#@doc").map(str::trim))
            .collect();
        let docs = if docs.is_empty() {
            format!("Project tool caos-tools/{filename} (no #@doc description).")
        } else {
            docs.join(" ")
        };
        tools.push(ToolDescription {
            name: name.to_string(),
            docs,
            image: TOOLS_IMAGE.to_string(),
        });
    }
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(ToolSetDescription { source, tools })
}

/// Structured progress from one turn. Frontends decide how to render these;
/// the harness never needs to know whether its caller is a pipe, a terminal,
/// or a full-screen UI.
#[derive(Clone, Debug, PartialEq)]
pub enum TurnEvent {
    PhaseStarted(TurnPhase),
    PhaseComplete {
        label: String,
        elapsed_secs: f64,
    },
    Status(String),
    AssistantText(String),
    ToolCall {
        step_commit: String,
        tool_use_id: String,
        name: String,
        summary: String,
    },
    ToolResult {
        step_commit: String,
        tool_use_id: String,
        is_error: bool,
        content: String,
    },
    Completed(TurnOutcome),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnPhase {
    System,
    Model,
}

/// The durable result of a successful turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnOutcome {
    pub conversation: String,
    pub commit: String,
    pub short_commit: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversationRole {
    Human,
    Agent,
}

/// One durable entry on the clean, first-parent conversation spine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationTurn {
    pub commit: String,
    pub short_commit: String,
    pub author: String,
    pub role: ConversationRole,
    pub message: String,
}

/// A locally-known conversation ref, ordered newest-first by
/// [`list_conversations`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationSummary {
    pub name: String,
    pub head: String,
    pub updated_unix: i64,
}

/// One conversation visible in a user's server-side active or archived index.
///
/// `id` is stable and addresses the conversation HEAD. `title` is mutable
/// presentation metadata stored under a separate ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserConversationSummary {
    pub id: String,
    pub title: String,
    pub head: String,
    pub updated_unix: i64,
}

/// A user's independent view of a conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserConversationStatus {
    Active,
    Archived,
}

impl UserConversationStatus {
    fn ref_component(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }
}

/// The accumulated workspace change carried by a conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceDiff {
    pub base_commit: String,
    pub head: String,
    pub patch: String,
}

/// Which verb is parsing: they share every flag, but the positional argument
/// is the conversation *name* for `chat` and the *prompt* for `talk`.
#[derive(PartialEq, Clone, Copy)]
enum Verb {
    Chat,
    Talk,
}

/// Parsed `chat`/`talk` arguments (see [`usage`]).
struct ChatArgs {
    /// `chat`'s positional / `talk`'s `-c`; `None` (talk only) = sticky pick.
    name: Option<String>,
    /// `-m` / `talk`'s positional; `None` = stdin, or the interactive loop.
    message: Option<String>,
    /// `talk --new`: start a fresh conversation instead of continuing.
    new_conv: bool,
    base: Option<String>,
    system: Option<String>,
    system_file: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    llm_step_bin: Option<String>,
    bash_tool_bin: Option<String>,
    rgrep_bin: Option<String>,
    log: bool,
}

fn usage(verb: Verb) -> String {
    let common = "[--base <revspec>] [--system <text> | --system-file <path>] \
         [--model <model>] [--base-url <url>] [--llm-step-bin <path>] \
         [--bash-tool-bin <path>] [--rgrep-bin <path>] [--log]";
    match verb {
        Verb::Chat => format!(
            "usage: chat <name> [-m <message>] {common}\n\
             One turn per invocation; the message is read from stdin without -m. \
             --log prints the conversation so far and runs nothing."
        ),
        Verb::Talk => format!(
            "usage: talk [<prompt>] [-c <name>] [--new] {common}\n\
             Continues this repo's most recent conversation (-c picks one, --new \
             starts another). With no <prompt>: interactive on a terminal, one \
             turn per line; otherwise the prompt is read from stdin. \
             --log prints the conversation so far and runs nothing."
        ),
    }
}

impl ChatArgs {
    fn parse(verb: Verb, args: &[String]) -> Result<ChatArgs, String> {
        let mut it = args.iter();
        let mut a = ChatArgs {
            name: None,
            message: None,
            new_conv: false,
            base: None,
            system: None,
            system_file: None,
            model: None,
            base_url: None,
            llm_step_bin: None,
            bash_tool_bin: None,
            rgrep_bin: None,
            log: false,
        };
        let mut positional: Option<String> = None;
        while let Some(arg) = it.next() {
            let mut value = |flag: &str| {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value\n{}", usage(verb)))
            };
            match arg.as_str() {
                "-m" | "--message" => a.message = Some(value(arg)?),
                "-c" | "--conversation" if verb == Verb::Talk => a.name = Some(value(arg)?),
                "--new" if verb == Verb::Talk => a.new_conv = true,
                "--base" => a.base = Some(value(arg)?),
                "--system" => a.system = Some(value(arg)?),
                "--system-file" => a.system_file = Some(value(arg)?),
                "--model" => a.model = Some(value(arg)?),
                "--base-url" => a.base_url = Some(value(arg)?),
                "--llm-step-bin" => a.llm_step_bin = Some(value(arg)?),
                "--bash-tool-bin" => a.bash_tool_bin = Some(value(arg)?),
                "--rgrep-bin" => a.rgrep_bin = Some(value(arg)?),
                "--log" => a.log = true,
                other if other.starts_with('-') => {
                    return Err(format!("unknown option {other}\n{}", usage(verb)))
                }
                _ if positional.is_none() => positional = Some(arg.clone()),
                other => {
                    let what = match verb {
                        Verb::Chat => "chat takes one <name>",
                        Verb::Talk => "talk takes one <prompt> (quote it)",
                    };
                    return Err(format!("{what}, got an extra: {other}\n{}", usage(verb)));
                }
            }
        }
        match verb {
            Verb::Chat => a.name = Some(positional.ok_or_else(|| usage(verb))?),
            Verb::Talk => match (positional, &a.message) {
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "the prompt was given both positionally and with -m\n{}",
                        usage(verb)
                    ))
                }
                (Some(p), None) => a.message = Some(p),
                (None, _) => {}
            },
        }
        if a.system.is_some() && a.system_file.is_some() {
            return Err("--system and --system-file are mutually exclusive".to_string());
        }
        Ok(a)
    }

    fn turn_options(&self) -> TurnOptions {
        TurnOptions {
            base: self.base.clone(),
            system: self.system.clone(),
            system_file: self.system_file.clone(),
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            llm_step_bin: self.llm_step_bin.clone(),
            bash_tool_bin: self.bash_tool_bin.clone(),
            rgrep_bin: self.rgrep_bin.clone(),
        }
    }
}

/// `chat <name> …` — the explicit, scriptable one-turn form.
pub fn cli_chat(t: &GitTransport, args: &[String]) -> Result<(), String> {
    let a = ChatArgs::parse(Verb::Chat, args)?;
    migrate_legacy_conversation_refs(t)?;
    let name = a.name.clone().expect("chat parse requires a name");
    let refname = validated_refname(&name)?;
    if a.log {
        return print_log(t, &name, &refname);
    }
    let message = read_message(a.message.as_deref())?;
    run_cli_turn(t, &a.turn_options(), &name, &message)
}

/// `talk [<prompt>] …` — the everyday surface; see the module documentation.
pub fn cli_talk(t: &GitTransport, args: &[String]) -> Result<(), String> {
    let a = ChatArgs::parse(Verb::Talk, args)?;
    migrate_legacy_conversation_refs(t)?;
    let (name, fresh) = pick_conversation(t, &a)?;
    let refname = validated_refname(&name)?;
    if a.log {
        return print_log(t, &name, &refname);
    }
    eprintln!("[conversation {name}{}]", if fresh { " — new" } else { "" });
    if let Some(prompt) = &a.message {
        return run_cli_turn(t, &a.turn_options(), &name, prompt);
    }
    if !std::io::stdin().is_terminal() {
        // Piped input: the whole of stdin is one prompt, one turn.
        let message = read_message(None)?;
        return run_cli_turn(t, &a.turn_options(), &name, &message);
    }
    // Interactive: one turn per line, until EOF (ctrl-d). A failed turn is
    // reported but doesn't end the session — the ref wasn't advanced, so the
    // next line simply retries from the same head.
    loop {
        eprint!("> ");
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => {
                eprintln!();
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => return Err(format!("reading from the terminal: {e}")),
        }
        let message = line.trim();
        if message.is_empty() {
            continue;
        }
        if let Err(e) = run_cli_turn(t, &a.turn_options(), &name, message) {
            eprintln!("talk: {e}");
        }
    }
}

/// Run one turn and preserve the existing line-oriented CLI presentation.
fn run_cli_turn(
    t: &GitTransport,
    options: &TurnOptions,
    name: &str,
    message: &str,
) -> Result<(), String> {
    run_chat_turn(t, options, name, message, None, |event| match event {
        TurnEvent::PhaseComplete {
            label,
            elapsed_secs,
        } if elapsed_secs >= 1.0 => eprintln!("· {label} took {elapsed_secs:.1}s"),
        TurnEvent::Status(text) => eprintln!("· {}", text.trim_end()),
        TurnEvent::AssistantText(text) => println!("{}", text.trim_end()),
        TurnEvent::ToolCall { summary, .. } => println!("{summary}"),
        TurnEvent::Completed(outcome) => {
            println!("[{} {}]", outcome.conversation, outcome.short_commit)
        }
        TurnEvent::PhaseStarted(_)
        | TurnEvent::PhaseComplete { .. }
        | TurnEvent::ToolResult { .. } => {}
    })?;
    Ok(())
}

/// Migrate legacy conversation refs into the `<id>/from-user` scheme, in
/// place and idempotently. A conversation head used to live at the bare
/// `refs/caos/conversations/<id>`; it now lives at `<id>/from-user`. Any ref
/// whose final segment is not a reserved channel is such a legacy bare head:
/// move it (a bare ref is a FILE where `<id>/from-user` needs `<id>` to be a
/// DIRECTORY, so the two cannot coexist — delete the bare ref, then create the
/// channel). A no-op once migrated (already-`from-user` refs are skipped), so
/// it is safe to run at every entry point.
fn migrate_legacy_conversation_refs(t: &GitTransport) -> Result<(), String> {
    let mut legacy = Vec::new();
    for (refname, oid) in t.local_refs(CONV_REF_PREFIX.trim_end_matches('/'))? {
        let Some(rest) = refname.strip_prefix(CONV_REF_PREFIX) else {
            continue;
        };
        let last_segment = rest.rsplit('/').next().unwrap_or(rest);
        if RESERVED_CHANNELS.contains(&last_segment) {
            continue; // already a channel ref (`from-user` and friends)
        }
        let id = rest.to_string();
        legacy.push((refname, id, oid));
    }
    for (old_ref, id, oid) in legacy {
        let new_ref = conv_ref(&id, FROM_USER_CHANNEL);
        // Delete only if still at the value we read, then create the channel.
        t.delete_local_ref(&old_ref, &oid)?;
        t.update_local_ref(&new_ref, &oid)?;
    }
    Ok(())
}

/// Rename a legacy server conversation head (`<id>/head`) to `<id>/from-user`
/// with one atomic push: create the channel at `head`, delete the legacy ref.
/// Idempotent in effect — once done, `<id>/head` is gone and later lists take
/// the `from-user` path.
fn migrate_server_conversation_head(t: &GitTransport, id: &str, head: &str) -> Result<(), String> {
    t.push_server_refs(&[
        (conversation_head_ref(id), Some(head.to_string())),
        (conversation_legacy_head_ref(id), None),
    ])
    .map_err(|error| format!("migrating conversation {id:?} server head: {error}"))
}

/// The conversation's `from-user` ref for `name` — the engine head — validated
/// up front. The id's final segment may not be a reserved channel name (else
/// `<id>/from-user` would collide with another conversation's channel), and git
/// must accept the full refname.
fn validated_refname(name: &str) -> Result<String, String> {
    let last_segment = name.rsplit('/').next().unwrap_or(name);
    if RESERVED_CHANNELS.contains(&last_segment) {
        return Err(format!(
            "conversation names ending in a channel segment {last_segment:?} are reserved"
        ));
    }
    let refname = conversation_head_ref(name);
    let _: &gix::refs::FullNameRef = refname
        .as_str()
        .try_into()
        .map_err(|_| format!("invalid conversation name {name:?}"))?;
    Ok(refname)
}

/// Which conversation a `talk` invocation is about, and whether it's new:
/// `-c <name>` names one (existing or not); `--new` mints a fresh auto-named
/// one; with neither, the repo's most recently advanced conversation — or a
/// fresh one when there is none yet.
fn pick_conversation(t: &GitTransport, a: &ChatArgs) -> Result<(String, bool), String> {
    if let Some(name) = &a.name {
        if a.new_conv && rev_parse_opt(t, &conversation_head_ref(name))?.is_some() {
            return Err(format!(
                "--new: conversation {name:?} already exists (drop --new to continue it)"
            ));
        }
        let fresh = rev_parse_opt(t, &conversation_head_ref(name))?.is_none();
        return Ok((name.clone(), fresh));
    }
    if !a.new_conv {
        if let Some(name) = latest_conversation(t)? {
            return Ok((name, false));
        }
    }
    let conversations = list_conversations(t)?;
    Ok((
        first_available_conversation_name(
            conversations
                .iter()
                .map(|conversation| conversation.name.as_str()),
        ),
        true,
    ))
}

/// Return the first unused auto-generated conversation name.
///
/// Name allocation is presentation-independent: every client uses the same
/// `talk-<n>` scheme and may include names that exist only in its current
/// session as well as durable conversation refs.
pub fn first_available_conversation_name<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let names: HashSet<&str> = names.into_iter().collect();
    for number in 1.. {
        let candidate = format!("{AUTO_NAME_PREFIX}{number}");
        if !names.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!("some talk-<n> is always free")
}

/// The most recently advanced conversation in this repo, by the head commit's
/// committer date (turn commits carry wall-clock timestamps).
fn latest_conversation(t: &GitTransport) -> Result<Option<String>, String> {
    Ok(list_conversations(t)?.into_iter().next().map(|c| c.name))
}

/// List the local conversations, newest first, by finding each `from-user`
/// (head) ref. The other channels (`from-agent`/`title`/`status`) are not
/// conversation heads and are skipped.
pub fn list_conversations(t: &GitTransport) -> Result<Vec<ConversationSummary>, String> {
    let head_suffix = format!("/{FROM_USER_CHANNEL}");
    let mut conversations = Vec::new();
    for (refname, head) in t.local_refs(CONV_REF_PREFIX.trim_end_matches('/'))? {
        let Some(rest) = refname.strip_prefix(CONV_REF_PREFIX) else {
            continue;
        };
        let Some(name) = rest.strip_suffix(&head_suffix) else {
            continue;
        };
        let updated_unix = t.commit_details(&head)?.timestamp;
        conversations.push(ConversationSummary {
            name: name.to_string(),
            head,
            updated_unix,
        });
    }
    conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.updated_unix));
    Ok(conversations)
}

const USER_CONVERSATION_PREFIX: &str = "refs/caos/users/";

/// The ref for one channel of a conversation.
fn conv_ref(id: &str, channel: &str) -> String {
    format!("{CONV_REF_PREFIX}{id}/{channel}")
}

fn conversation_head_ref(id: &str) -> String {
    conv_ref(id, FROM_USER_CHANNEL)
}

/// The pre-`from-user` server head channel. A conversation an older TUI
/// published lives only here until [`list_user_conversations`] renames it.
fn conversation_legacy_head_ref(id: &str) -> String {
    conv_ref(id, "head")
}

fn conversation_title_ref(id: &str) -> String {
    conv_ref(id, TITLE_CHANNEL)
}

fn user_conversation_ref(user: &str, status: UserConversationStatus, id: &str) -> String {
    format!(
        "{USER_CONVERSATION_PREFIX}{user}/conversations/{}/{id}",
        status.ref_component()
    )
}

fn validate_conversation_user(user: &str) -> Result<(), String> {
    let refname = user_conversation_ref(user, UserConversationStatus::Active, "conversation");
    <&gix::refs::FullNameRef>::try_from(refname.as_str())
        .map(|_| ())
        .map_err(|_| format!("invalid conversation user {user:?}"))
}

fn validate_user_conversation(user: &str, id: &str) -> Result<(), String> {
    validated_refname(id)?;
    validate_conversation_user(user)
}

fn validate_conversation_title(title: &str) -> Result<&str, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("conversation title cannot be empty".to_string());
    }
    if title.contains(['\n', '\r', '\t']) {
        return Err("conversation title must be one line".to_string());
    }
    Ok(title)
}

fn remote_refs(
    t: &GitTransport,
    patterns: impl IntoIterator<Item = String>,
) -> Result<HashMap<String, String>, String> {
    let patterns: Vec<String> = patterns.into_iter().collect();
    if patterns.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(t.server_refs(&patterns)?.into_iter().collect())
}

/// Publish a local conversation into the server-owned conversation namespace
/// and mark it active for `user`.
///
/// The stable `id` remains in every ref path. Updating `title` changes only its
/// metadata ref; advancing the conversation changes only its HEAD ref.
pub fn publish_user_conversation(
    t: &GitTransport,
    user: &str,
    id: &str,
    title: &str,
) -> Result<(), String> {
    validate_user_conversation(user, id)?;
    let title = validate_conversation_title(title)?;
    let local_ref = validated_refname(id)?;
    let head = rev_parse_opt(t, &local_ref)?
        .ok_or_else(|| format!("cannot publish conversation {id:?} before its first turn"))?;
    let title_hash = t.put_object("blob", title.as_bytes())?.to_string();
    t.ensure_pushed(&title_hash)?;

    let head_ref = conversation_head_ref(id);
    let title_ref = conversation_title_ref(id);
    let active_ref = user_conversation_ref(user, UserConversationStatus::Active, id);
    t.push_server_refs(&[
        (head_ref, Some(head.clone())),
        (title_ref, Some(title_hash)),
        (active_ref, Some(head)),
    ])
    .map_err(|error| format!("publishing conversation {id:?}: {error}"))
}

/// Change a conversation's shared title without changing its identity or HEAD.
pub fn set_conversation_title(t: &GitTransport, id: &str, title: &str) -> Result<(), String> {
    validated_refname(id)?;
    let title = validate_conversation_title(title)?;
    let hash = t.put_object("blob", title.as_bytes())?.to_string();
    t.ensure_pushed(&hash)?;
    let title_ref = conversation_title_ref(id);
    t.push_server_refs(&[(title_ref, Some(hash))])
        .map_err(|error| format!("renaming conversation {id:?}: {error}"))
}

fn move_user_conversation(
    t: &GitTransport,
    user: &str,
    id: &str,
    from: UserConversationStatus,
    to: UserConversationStatus,
) -> Result<(), String> {
    validate_user_conversation(user, id)?;
    let from_ref = user_conversation_ref(user, from, id);
    let to_ref = user_conversation_ref(user, to, id);
    let refs = remote_refs(t, [from_ref.clone(), to_ref.clone()])?;
    match (refs.get(&from_ref), refs.get(&to_ref)) {
        (None, Some(_)) => Ok(()),
        (None, None) => Err(format!(
            "conversation {id:?} is not {} for user {user:?}",
            from.ref_component()
        )),
        (Some(_), Some(_)) => Err(format!(
            "conversation {id:?} is both active and archived for user {user:?}"
        )),
        (Some(hash), None) => t
            .push_server_refs(&[(to_ref, Some(hash.clone())), (from_ref, None)])
            .map_err(|error| {
                format!(
                    "moving conversation {id:?} from {} to {}: {error}",
                    from.ref_component(),
                    to.ref_component()
                )
            }),
    }
}

/// Archive one conversation for one user. The canonical HEAD and every other
/// user's state are untouched.
pub fn archive_user_conversation(t: &GitTransport, user: &str, id: &str) -> Result<(), String> {
    move_user_conversation(
        t,
        user,
        id,
        UserConversationStatus::Active,
        UserConversationStatus::Archived,
    )
}

/// Restore one archived conversation to a user's active list.
pub fn unarchive_user_conversation(t: &GitTransport, user: &str, id: &str) -> Result<(), String> {
    move_user_conversation(
        t,
        user,
        id,
        UserConversationStatus::Archived,
        UserConversationStatus::Active,
    )
}

/// Import legacy local conversation refs into a user's server-side index once.
///
/// Existing active or archived entries win, so opening the TUI never
/// accidentally unarchives a conversation.
pub fn publish_unindexed_conversations(t: &GitTransport, user: &str) -> Result<(), String> {
    validate_conversation_user(user)?;
    migrate_legacy_conversation_refs(t)?;
    let active = format!(
        "{USER_CONVERSATION_PREFIX}{user}/conversations/{}/{}",
        UserConversationStatus::Active.ref_component(),
        "*"
    );
    let archived = format!(
        "{USER_CONVERSATION_PREFIX}{user}/conversations/{}/{}",
        UserConversationStatus::Archived.ref_component(),
        "*"
    );
    let indexed = remote_refs(t, [active, archived])?;
    for conversation in list_conversations(t)? {
        validate_user_conversation(user, &conversation.name)?;
        let active_ref =
            user_conversation_ref(user, UserConversationStatus::Active, &conversation.name);
        let archived_ref =
            user_conversation_ref(user, UserConversationStatus::Archived, &conversation.name);
        if !indexed.contains_key(&active_ref) && !indexed.contains_key(&archived_ref) {
            publish_user_conversation(t, user, &conversation.name, &conversation.name)?;
        }
    }
    Ok(())
}

/// List one user's active or archived conversations from authoritative remote
/// refs, then refresh their local engine refs as a cache for transcript reads.
pub fn list_user_conversations(
    t: &GitTransport,
    user: &str,
    status: UserConversationStatus,
) -> Result<Vec<UserConversationSummary>, String> {
    validate_conversation_user(user)?;
    let prefix = format!(
        "{USER_CONVERSATION_PREFIX}{user}/conversations/{}/",
        status.ref_component()
    );
    let state_refs = remote_refs(t, [format!("{prefix}*")])?;
    let ids: Vec<String> = state_refs
        .keys()
        .filter_map(|refname| refname.strip_prefix(&prefix).map(str::to_string))
        .collect();
    for id in &ids {
        validate_user_conversation(user, id)?;
    }

    let mut metadata = HashMap::new();
    for chunk in ids.chunks(128) {
        let mut patterns = Vec::new();
        for id in chunk {
            patterns.push(conversation_head_ref(id));
            patterns.push(conversation_title_ref(id));
            patterns.push(conversation_legacy_head_ref(id));
        }
        metadata.extend(remote_refs(t, patterns)?);
    }
    let mut conversations = Vec::new();
    for id in ids {
        // Prefer the `from-user` head; a conversation an older TUI published
        // lives only at the legacy `<id>/head` — rename it on the server the
        // first time it is listed, then use it.
        let head = match metadata.get(&conversation_head_ref(&id)) {
            Some(head) => head.clone(),
            None => {
                let legacy = metadata
                    .get(&conversation_legacy_head_ref(&id))
                    .ok_or_else(|| format!("conversation {id:?} has no server HEAD"))?
                    .clone();
                migrate_server_conversation_head(t, &id, &legacy)?;
                legacy
            }
        };
        t.fetch_object(&head)?;
        let local_ref = validated_refname(&id)?;
        t.update_local_ref(&local_ref, &head)?;

        let title_ref = conversation_title_ref(&id);
        let title_hash = metadata
            .get(&title_ref)
            .ok_or_else(|| format!("conversation {id:?} has no title"))?;
        let (kind, title) = t.get_object(title_hash)?;
        if kind != "blob" {
            return Err(format!(
                "conversation title {title_hash} is a {kind}, not a blob"
            ));
        }
        let title = String::from_utf8(title)
            .map_err(|_| format!("conversation {id:?} title is not UTF-8"))?;
        let updated_unix = t.commit_details(&head)?.timestamp;
        conversations.push(UserConversationSummary {
            id,
            title,
            head,
            updated_unix,
        });
    }
    conversations.sort_by(|a, b| {
        b.updated_unix
            .cmp(&a.updated_unix)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(conversations)
}

/// Read a named conversation's clean human/agent spine, oldest first.
pub fn conversation_history(t: &GitTransport, name: &str) -> Result<Vec<ConversationTurn>, String> {
    let refname = validated_refname(name)?;
    let head = rev_parse_opt(t, &refname)?
        .ok_or_else(|| format!("no conversation {name:?} ({refname} not found)"))?;
    history_from_head(t, &head).map(|(turns, _base)| turns)
}

/// Diff the conversation's current workspace against the commit it started
/// from. This operation is side-effect free; clients own any policy for
/// applying or publishing the returned change.
pub fn conversation_workspace_diff(t: &GitTransport, name: &str) -> Result<WorkspaceDiff, String> {
    let refname = validated_refname(name)?;
    let head = rev_parse_opt(t, &refname)?
        .ok_or_else(|| format!("no conversation {name:?} ({refname} not found)"))?;
    let (_turns, base_commit) = history_from_head(t, &head)?;
    let patch = t.diff_commits(&base_commit, &head)?;
    Ok(WorkspaceDiff {
        base_commit,
        head,
        patch,
    })
}

/// Run one conversation turn, emitting structured progress as it happens.
///
/// The callback runs on the calling thread. A full-screen client will normally
/// call this function from a worker thread and forward the events over a
/// channel to its terminal event loop.
pub fn run_chat_turn(
    t: &GitTransport,
    options: &TurnOptions,
    name: &str,
    message: &str,
    human_tree: Option<&str>,
    mut emit: impl FnMut(TurnEvent),
) -> Result<TurnOutcome, String> {
    let refname = validated_refname(name)?;
    if message.trim().is_empty() {
        return Err("empty message".to_string());
    }
    emit(TurnEvent::PhaseStarted(TurnPhase::System));
    turn(
        t,
        options,
        name,
        &refname,
        message.trim(),
        human_tree,
        &mut emit,
    )
}

/// One turn: mint the human commit, run llm-step over it, emit progress, and
/// advance the conversation ref.
///
/// `human_tree`, when set, is the tree the human commit carries instead of
/// inheriting the parent's — this is how a client folds local working-tree
/// edits into a user-authored turn (the TUI's `/update-tree`).
fn turn(
    t: &GitTransport,
    options: &TurnOptions,
    name: &str,
    refname: &str,
    message: &str,
    human_tree: Option<&str>,
    emit: &mut dyn FnMut(TurnEvent),
) -> Result<TurnOutcome, String> {
    // Everything that can fail cheaply fails *before* the human commit is
    // minted or anything is pushed.
    let api_key = std::env::var(API_KEY_ENV).map_err(|_| {
        format!("{API_KEY_ENV} must be set (it rides, curried, into the llm-step run)")
    })?;
    let llm_bin = worker_bin(options.llm_step_bin.as_deref(), LLM_STEP_BIN_ENV);
    let bash_bin = worker_bin(options.bash_tool_bin.as_deref(), BASH_TOOL_BIN_ENV);
    let rgrep_bin = worker_bin(options.rgrep_bin.as_deref(), RGREP_BIN_ENV);
    let system = match (&options.system, &options.system_file) {
        (Some(text), _) => text.clone(),
        (None, Some(path)) => {
            std::fs::read_to_string(path).map_err(|e| format!("--system-file {path}: {e}"))?
        }
        (None, None) => DEFAULT_SYSTEM.to_string(),
    };

    // The human commit's parent: the conversation head, or — for a new
    // conversation — the base commit (HEAD unless --base overrides).
    let parent = match rev_parse_opt(t, refname)? {
        Some(head) => head,
        None => {
            let rev = options.base.as_deref().unwrap_or("HEAD");
            let base = t
                .resolve_revspec(rev)?
                .ok_or_else(|| format!("cannot resolve --base {rev:?}"))?
                .to_string();
            // `.caos` is the harness's reserved top-level workspace entry
            // (step transcripts live there): refuse to start a conversation
            // over a tree that already carries one.
            if rev_parse_opt(t, &format!("{base}:.caos"))?.is_some() {
                return Err(
                    "the base commit's tree contains a top-level `.caos` entry, which \
                     is reserved for the agent harness; start from a tree without one"
                        .to_string(),
                );
            }
            base
        }
    };

    // The agent author name is the turn-walk marker; a human commit carrying it
    // would corrupt every future transcript walk.
    let author = t.author_name()?;
    if author == AGENT_AUTHOR {
        return Err(format!(
            "your git author name is {AGENT_AUTHOR:?}, which is reserved for agent commits; \
             set a different user.name"
        ));
    }

    // Mint the human turn: parent = head/base, message = the user's text,
    // author = the user's git identity. The tree is the parent's (human turns
    // are text-only) unless the client supplied one — a working-tree snapshot
    // folded in by `/update-tree`, which must not carry the harness's reserved
    // top-level `.caos` entry.
    let tree = match human_tree {
        Some(tree) => {
            if rev_parse_opt(t, &format!("{tree}:.caos"))?.is_some() {
                return Err(
                    "the working tree contains a top-level `.caos` entry, which is reserved \
                     for the agent harness; remove it before folding the tree into a turn"
                        .to_string(),
                );
            }
            tree.to_string()
        }
        None => t.commit_details(&parent)?.tree,
    };
    let human = t.create_commit(&tree, &parent, message)?;

    // The workers: by default the std-published curries (`curry(runner, bin)`,
    // build-builtins.sh) — already server-side under refs/caos/std, nothing to
    // build or push. An explicit `--*-bin` override (the stub tests' path)
    // curries that binary onto the runner-pool image here instead; the bash
    // curry's hash is passed to llm-step as a *literal* (an image ref string),
    // so its closure doesn't ride in the request graph — push it (and the
    // runner image) explicitly.
    let phase = std::time::Instant::now();
    let runner = match (&llm_bin, &bash_bin, &rgrep_bin) {
        (None, None, None) => None,
        _ => Some(resolve_cli_image(t, RUNNER_IMAGE)?),
    };
    let bash_image = match &bash_bin {
        Some(bin) => {
            let runner = runner.as_deref().expect("resolved when a bin is given");
            let img =
                curry_object(t, runner, None, &[], &[format!("--worker1:@={bin}")])?.to_string();
            t.ensure_pushed(&img)?;
            t.ensure_pushed(runner)?;
            img
        }
        None => resolve_cli_image(t, BASH_TOOL_IMAGE)?,
    };

    let grep_image = match &rgrep_bin {
        Some(bin) => {
            let runner = runner.as_deref().expect("resolved when a bin is given");
            let img =
                curry_object(t, runner, None, &[], &[format!("--worker1:@={bin}")])?.to_string();
            t.ensure_pushed(&img)?;
            t.ensure_pushed(runner)?;
            img
        }
        None => resolve_cli_image(t, RGREP_IMAGE)?,
    };

    // Optional: a stack whose std predates the bash script worker simply
    // doesn't register tree tools (llm-step treats a missing tools_image
    // that way too).
    let tools_image = resolve_cli_image(t, TOOLS_IMAGE).ok();

    // The merge worker and the turn-start ref snapshot (SPEC "Resolving
    // `--theirs`"): resolve a curated set of refs to hashes, push each closure
    // (onto its content-addressed `refs/caos/req/<hash>`, so no semantic ref is
    // written to the shared server), and carry a name→hash map into the turn.
    // ensure_pushed prunes closures the server already has, so an unmoved ref
    // uploads nothing.
    let merge_image = resolve_cli_image(t, MERGE_IMAGE).ok();
    let merge_refs = match &merge_image {
        None => None,
        Some(_) => {
            let mut lines = String::new();
            for spec in MERGE_REF_CANDIDATES {
                if let Some(hash) = rev_parse_opt(t, spec)? {
                    t.ensure_pushed(&hash)?;
                    lines.push_str(&format!("{spec} {hash}\n"));
                }
            }
            Some(lines)
        }
    };

    let mut kvs = vec![
        format!("--api-key={api_key}"),
        format!("--system={system}"),
        format!("--bash-image={bash_image}"),
        format!("--grep-image={grep_image}"),
        format!("--conversation={name}"),
    ];
    if let Some(tools) = &tools_image {
        kvs.push(format!("--tools-image={tools}"));
    }
    if let Some(merge) = &merge_image {
        kvs.push(format!("--merge-image={merge}"));
    }
    if let Some(refs) = &merge_refs {
        kvs.push(format!("--merge-refs={refs}"));
    }
    if let Some(model) = &options.model {
        kvs.push(format!("--model={model}"));
    }
    if let Some(url) = &options.base_url {
        kvs.push(format!("--base-url={url}"));
    }
    // Per-turn state currying: onto the std llm-step curry (layers flatten, so
    // the result is exactly curry(runner, bin, <state>)), or onto the runner
    // with the override binary.
    let llm_base = match &llm_bin {
        Some(bin) => {
            kvs.push(format!("--worker1:@={bin}"));
            runner.clone().expect("resolved when a bin is given")
        }
        None => resolve_cli_image(t, LLM_STEP_IMAGE)?,
    };
    let llm = curry_object(t, &llm_base, None, &[], &kvs)?.to_string();
    emit(TurnEvent::PhaseComplete {
        label: "resolving the workers".to_string(),
        elapsed_secs: phase.elapsed().as_secs_f64(),
    });

    // Build + push the request (this also pushes the human commit's closure —
    // the `:commit=` machinery), then trigger the blocking compute on its own
    // thread: request_compute needs only two strings, so the transport (and
    // the repo handle) stay on this thread for progress polling.
    let phase = std::time::Instant::now();
    let arg_tree = prepare_request(t, &llm, None, &[format!("--head:commit={human}")])?;
    emit(TurnEvent::PhaseComplete {
        label: "pushing the turn".to_string(),
        elapsed_secs: phase.elapsed().as_secs_f64(),
    });
    emit(TurnEvent::PhaseStarted(TurnPhase::Model));
    let server = t.server_url()?;
    let run = {
        let (server, arg_tree) = (server.clone(), arg_tree);
        std::thread::spawn(move || request_compute(&server, &arg_tree))
    };

    // While the run blocks, follow the worker's per-step progress ref and
    // print each new step (assistant text + one-line tool calls); alongside
    // it, the in-round status ref — what the API call is doing right now —
    // goes to stderr (transient meta, not conversation content).
    let http = HttpTransport { base: server };
    let progress_ref = conv_ref(name, FROM_AGENT_CHANNEL);
    let status_ref = conv_ref(name, STATUS_CHANNEL);
    let mut printed: HashSet<String> = HashSet::new();
    let mut last_status: Option<String> = None;
    while !run.is_finished() {
        for _ in 0..(POLL_MS / 100) {
            if run.is_finished() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if run.is_finished() {
            break;
        }
        if let Err(e) = poll_progress(t, &http, &progress_ref, &human, &mut printed, emit) {
            emit(TurnEvent::Status(format!(
                "progress poll failed (non-fatal): {e}"
            )));
        }
        // Best-effort by design, like the ref it reads.
        let _ = poll_status(t, &http, &status_ref, &human, &mut last_status, emit);
    }

    let outcome = run
        .join()
        .map_err(|_| "the run thread panicked".to_string())?;
    let (kind, turn_hash) = match outcome {
        Ok(result) => result,
        Err(e) => {
            // Show whatever steps did land before the failure, then fail; the
            // conversation ref is untouched (the human commit is harmlessly
            // orphaned — see design/agent-harness.md).
            let _ = poll_progress(t, &http, &progress_ref, &human, &mut printed, emit);
            return Err(format!("turn failed; {refname} was not advanced.\n{e}"));
        }
    };
    if kind != "commit" {
        return Err(format!("the run returned a {kind}, expected a commit"));
    }

    // Fetch the turn (and so the whole step chain — it's tree-reachable), then
    // drain any steps a poll didn't catch. The final step's text blocks ARE the
    // turn message, so the response is printed exactly once: either a poll
    // already showed the final step (skip the message), or the drain here
    // suppresses that step's text and the message is printed below.
    // Negotiate from the human commit, which the server already has, so the
    // transfer contains only this turn's new objects.
    emit(TurnEvent::PhaseStarted(TurnPhase::System));
    let phase = std::time::Instant::now();
    t.fetch_object_negotiated(&turn_hash, &human)?;
    emit(TurnEvent::PhaseComplete {
        label: "fetching the turn".to_string(),
        elapsed_secs: phase.elapsed().as_secs_f64(),
    });
    let mut show_message = true;
    if let Some(tail) = rev_parse_opt(t, &format!("{turn_hash}^2"))? {
        if printed.contains(&tail) {
            show_message = false;
        } else {
            let _ = drain_steps(&http, &tail, &human, &mut printed, Some(&tail), emit);
        }
    }

    t.update_local_ref(refname, &turn_hash)?;
    let details = t.commit_details(&turn_hash)?;
    let text = details.message;
    let short = details.short_id;
    if show_message {
        emit(TurnEvent::AssistantText(text.trim_end().to_string()));
    }
    let outcome = TurnOutcome {
        conversation: name.to_string(),
        commit: turn_hash,
        short_commit: short,
    };
    emit(TurnEvent::Completed(outcome.clone()));
    Ok(outcome)
}

/// An explicit worker-binary override: the flag, else its env var, else `None`
/// — the std-published curry is used.
fn worker_bin(flag_value: Option<&str>, env: &str) -> Option<String> {
    flag_value
        .map(str::to_string)
        .or_else(|| std::env::var(env).ok())
}

/// The turn's message: the given one, or stdin read to EOF.
fn read_message(message: Option<&str>) -> Result<String, String> {
    let raw = match message {
        Some(m) => m.to_string(),
        None => {
            if std::io::stdin().is_terminal() {
                eprintln!("reading the message from stdin — end with EOF (ctrl-d)");
            }
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| format!("reading the message from stdin: {e}"))?;
            s
        }
    };
    let message = raw.trim().to_string();
    if message.is_empty() {
        return Err("empty message (pass -m <message> or write one to stdin)".to_string());
    }
    Ok(message)
}

fn rev_parse_opt(t: &GitTransport, spec: &str) -> Result<Option<String>, String> {
    t.resolve(spec)
}

// ---------------------------------------------------------------------------
// Progress: follow the conversation's `from-agent` ref while the run blocks.
// ---------------------------------------------------------------------------

/// One poll: read the progress ref off the server and print any new steps.
/// The ref not existing yet (first round still in flight) is normal.
fn poll_progress(
    t: &GitTransport,
    http: &HttpTransport,
    progress_ref: &str,
    human: &str,
    printed: &mut HashSet<String>,
    emit: &mut dyn FnMut(TurnEvent),
) -> Result<(), String> {
    let Some(tip) = t.server_ref(progress_ref)? else {
        return Ok(()); // no ref yet
    };
    drain_steps(http, &tip, human, printed, None, emit)
}

/// One poll of the in-round status ref: print this turn's newest status line
/// to stderr, once. The blob is `"<human hash>\n<text>"` — a first line that
/// isn't this turn's human commit is a previous turn's stale status. `last`
/// tracks the printed blob's hash (same hash = same text = already shown).
fn poll_status(
    t: &GitTransport,
    http: &HttpTransport,
    status_ref: &str,
    human: &str,
    last: &mut Option<String>,
    emit: &mut dyn FnMut(TurnEvent),
) -> Result<(), String> {
    let Some(tip) = t.server_ref(status_ref)? else {
        return Ok(()); // no ref yet
    };
    if last.as_deref() == Some(tip.as_str()) {
        return Ok(());
    }
    let (kind, content) = http.get_object(&tip)?;
    if kind != "blob" {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&content);
    let Some((turn_root, line)) = text.split_once('\n') else {
        return Ok(());
    };
    if turn_root == human {
        emit(TurnEvent::Status(line.trim_end().to_string()));
    }
    *last = Some(tip);
    Ok(())
}

/// Walk the step chain down from `tip` to the first known commit (`human`, or
/// one already printed) and print the new steps oldest-first. Objects are read
/// over the server's object API — mid-turn step commits are unreferenced
/// server-side objects, and nothing here needs to land in the local repo. A
/// chain that roots anywhere else is stale (e.g. the previous turn's ref,
/// still up while this turn's first step is in flight) and prints nothing.
/// `suppress_text` names a step whose text blocks are skipped (the final step
/// of a completed turn — its text is the turn message, printed separately).
fn drain_steps(
    http: &HttpTransport,
    tip: &str,
    human: &str,
    printed: &mut HashSet<String>,
    suppress_text: Option<&str>,
    emit: &mut dyn FnMut(TurnEvent),
) -> Result<(), String> {
    let mut chain: Vec<(String, Value)> = Vec::new();
    let mut cur = tip.to_string();
    loop {
        if cur == human || printed.contains(&cur) {
            break; // known root: everything collected is this turn's, print it
        }
        let (author, tree, first_parent) = commit_bits(http, &cur)?;
        if author != AGENT_AUTHOR {
            return Ok(()); // stale chain (roots at some other human commit)
        }
        chain.push((cur.clone(), step_json(http, &tree)?));
        match first_parent {
            Some(parent) => cur = parent,
            None => return Ok(()), // parentless — not this turn's chain
        }
    }
    for (hash, step) in chain.into_iter().rev() {
        emit_step(&step, &hash, suppress_text == Some(hash.as_str()), emit);
        printed.insert(hash);
    }
    Ok(())
}

/// A commit's `(author name, tree, first parent)` read over the object API.
fn commit_bits(
    http: &HttpTransport,
    hash: &str,
) -> Result<(String, String, Option<String>), String> {
    let (kind, content) = http.get_object(hash)?;
    if kind != "commit" {
        return Err(format!("{hash} is a {kind}, not a commit"));
    }
    let text = String::from_utf8_lossy(&content);
    let headers = text.split("\n\n").next().unwrap_or("");
    let (mut tree, mut parent, mut author) = (None, None, String::new());
    for line in headers.lines() {
        if let Some(hash) = line.strip_prefix("tree ") {
            tree = Some(hash.to_string());
        } else if let Some(hash) = line.strip_prefix("parent ") {
            parent.get_or_insert_with(|| hash.to_string());
        } else if let Some(ident) = line.strip_prefix("author ") {
            author = ident
                .split_once(" <")
                .map(|(name, _)| name)
                .unwrap_or(ident)
                .to_string();
        }
    }
    let tree = tree.ok_or_else(|| format!("commit {hash} has no tree line"))?;
    Ok((author, tree, parent))
}

/// A step commit's parsed `.caos/step.json`, read from its tree over the
/// object API (tree → `.caos` subtree → `step.json` blob).
fn step_json(http: &HttpTransport, tree: &str) -> Result<Value, String> {
    let entry = |tree: &str, name: &str| -> Result<String, String> {
        let (kind, content) = http.get_object(tree)?;
        if kind != "tree" {
            return Err(format!("{tree} is a {kind}, not a tree"));
        }
        let parsed = gix::objs::TreeRef::from_bytes(&content, gix::hash::Kind::Sha1)
            .map_err(|e| format!("malformed tree {tree}: {e}"))?;
        parsed
            .entries
            .iter()
            .find(|e| e.filename.to_vec().as_slice() == name.as_bytes())
            .map(|e| e.oid.to_string())
            .ok_or_else(|| format!("step tree {tree} has no {name:?} entry"))
    };
    let caos_tree = entry(tree, ".caos")?;
    let blob = entry(&caos_tree, "step.json")?;
    let (_, content) = http.get_object(&blob)?;
    serde_json::from_slice(&content).map_err(|e| format!("parsing step.json: {e}"))
}

/// Decode one durable step into frontend events. Thinking blocks stay private.
fn emit_step(
    step: &Value,
    step_commit: &str,
    suppress_text: bool,
    emit: &mut dyn FnMut(TurnEvent),
) {
    if let Some(results) = step["results"].as_array() {
        for result in results {
            let tool_use_id = result["tool_use_id"].as_str().unwrap_or("?").to_string();
            let is_error = result["is_error"].as_bool().unwrap_or(false);
            let content = block_text(&result["content"]);
            emit(TurnEvent::ToolResult {
                step_commit: step_commit.to_string(),
                tool_use_id,
                is_error,
                content,
            });
        }
    }
    let Some(blocks) = step["content"].as_array() else {
        return;
    };
    for block in blocks {
        match block["type"].as_str() {
            Some("text") if !suppress_text => {
                let text = block["text"].as_str().unwrap_or("").trim_end();
                if !text.is_empty() {
                    emit(TurnEvent::AssistantText(text.to_string()));
                }
            }
            Some("tool_use") => {
                let name = block["name"].as_str().unwrap_or("?");
                let summary = match name {
                    "bash" => format!("$ {}", block["input"]["cmd"].as_str().unwrap_or("?")),
                    name @ ("read" | "write" | "edit") => format!(
                        "{name} {}",
                        block["input"]["file_path"].as_str().unwrap_or("?")
                    ),
                    "ls" => format!("ls {}", block["input"]["path"].as_str().unwrap_or(".")),
                    "grep" => {
                        let pattern = block["input"]["pattern"].as_str().unwrap_or("?");
                        match block["input"]["path"].as_str() {
                            Some(path) => format!("grep {pattern} {path}"),
                            None => format!("grep {pattern}"),
                        }
                    }
                    other => format!("[tool call: {other}]"),
                };
                emit(TurnEvent::ToolCall {
                    step_commit: step_commit.to_string(),
                    tool_use_id: block["id"].as_str().unwrap_or("?").to_string(),
                    name: name.to_string(),
                    summary,
                });
            }
            _ => {}
        }
    }
}

fn block_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// --log: the conversation so far, from the local ref.
// ---------------------------------------------------------------------------

/// Print the conversation's turns oldest-first: a first-parent walk down from
/// the head. Below a turn commit sits its human turn; below a human turn,
/// either the previous (agent-authored) turn or the base commit — which ends
/// the conversation (design/agent-harness.md, "Commit structure").
fn print_log(t: &GitTransport, name: &str, refname: &str) -> Result<(), String> {
    let head = rev_parse_opt(t, refname)?
        .ok_or_else(|| format!("no conversation {name:?} ({refname} not found)"))?;
    let (turns, _base) = history_from_head(t, &head)?;
    for turn in turns {
        println!("── {} {}", turn.short_commit, turn.author);
        println!("{}", turn.message);
        println!();
    }
    Ok(())
}

/// Return the clean conversation and the base commit immediately beneath it.
fn history_from_head(
    t: &GitTransport,
    head: &str,
) -> Result<(Vec<ConversationTurn>, String), String> {
    let mut turns = Vec::new();
    let mut cur = head.to_string();
    let mut prev_was_agent = false;
    loop {
        let details = t.commit_details(&cur)?;
        let author = details.author;
        let is_agent = author == AGENT_AUTHOR;
        if !is_agent && !prev_was_agent {
            turns.reverse();
            return Ok((turns, cur)); // the base commit — conversation starts above it
        }
        let short = details.short_id;
        let message = details.message.trim_end().to_string();
        turns.push(ConversationTurn {
            commit: cur.clone(),
            short_commit: short,
            author,
            role: if is_agent {
                ConversationRole::Agent
            } else {
                ConversationRole::Human
            },
            message,
        });
        let Some(parent) = details.parent else {
            return Err(format!(
                "conversation rooted at {cur} has no distinct base commit"
            ));
        };
        prev_was_agent = is_agent;
        cur = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CAOS_REMOTE;
    use serde_json::json;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) -> String {
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
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn conversation_repo() -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "caos-user-conversations-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = root.join("repo");
        let remote = root.join("remote.git");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        git(&repo, &["init", "--quiet"]);
        git(&remote, &["init", "--quiet", "--bare"]);
        git(&repo, &["config", "user.name", "tester"]);
        git(&repo, &["config", "user.email", "tester@example.com"]);
        std::fs::write(repo.join("file"), "one\n").unwrap();
        git(&repo, &["add", "file"]);
        git(&repo, &["commit", "--quiet", "-m", "one"]);
        let first = git(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("file"), "two\n").unwrap();
        git(&repo, &["commit", "--quiet", "-am", "two"]);
        let second = git(&repo, &["rev-parse", "HEAD"]);
        let first_ref = "refs/caos/conversations/first/from-user";
        let second_ref = "refs/caos/conversations/second/from-user";
        git(&repo, &["update-ref", first_ref, &first]);
        git(&repo, &["update-ref", second_ref, &second]);
        git(
            &repo,
            &["remote", "add", CAOS_REMOTE, remote.to_str().unwrap()],
        );
        (root, repo)
    }

    #[test]
    fn conversation_ref_validation_does_not_spawn_git() {
        assert_eq!(
            validated_refname("talk-1").unwrap(),
            "refs/caos/conversations/talk-1/from-user"
        );
        assert!(validated_refname("bad name").is_err());
        assert!(validate_conversation_user("nishadsingh").is_ok());
        assert!(validate_conversation_user("bad user").is_err());
    }

    #[test]
    fn step_decoding_emits_results_text_and_tool_calls() {
        let step = json!({
            "results": [{
                "type": "tool_result",
                "tool_use_id": "tool-0",
                "content": [{"type": "text", "text": "exit: 0\nstdout:\nok"}]
            }],
            "content": [
                {"type": "thinking", "thinking": "private"},
                {"type": "text", "text": "working"},
                {
                    "type": "tool_use",
                    "id": "tool-1",
                    "name": "bash",
                    "input": {"cmd": "cargo test"}
                }
            ]
        });
        let mut events = Vec::new();
        emit_step(&step, "1234567890abcdef", false, &mut |event| {
            events.push(event)
        });
        assert_eq!(
            events,
            vec![
                TurnEvent::ToolResult {
                    step_commit: "1234567890abcdef".to_string(),
                    tool_use_id: "tool-0".to_string(),
                    is_error: false,
                    content: "exit: 0\nstdout:\nok".to_string(),
                },
                TurnEvent::AssistantText("working".to_string()),
                TurnEvent::ToolCall {
                    step_commit: "1234567890abcdef".to_string(),
                    tool_use_id: "tool-1".to_string(),
                    name: "bash".to_string(),
                    summary: "$ cargo test".to_string(),
                },
            ]
        );
    }

    #[test]
    fn final_step_can_suppress_duplicate_text_without_hiding_results() {
        let step = json!({
            "results": [{
                "tool_use_id": "tool-1",
                "is_error": true,
                "content": "failed"
            }],
            "content": [{"type": "text", "text": "final answer"}]
        });
        let mut events = Vec::new();
        emit_step(&step, "abcdef1234567890", true, &mut |event| {
            events.push(event)
        });
        assert_eq!(
            events,
            vec![TurnEvent::ToolResult {
                step_commit: "abcdef1234567890".to_string(),
                tool_use_id: "tool-1".to_string(),
                is_error: true,
                content: "failed".to_string(),
            }]
        );
    }

    #[test]
    fn auto_names_are_allocated_from_shared_conversation_policy() {
        assert_eq!(first_available_conversation_name([]), "talk-1");
        assert_eq!(
            first_available_conversation_name(["talk-1", "named", "talk-2"]),
            "talk-3"
        );
    }

    #[test]
    fn remote_user_state_lists_renames_archives_and_restores_conversations() {
        let (root, repo) = conversation_repo();
        let transport = GitTransport::discover(&repo).unwrap();
        let first_ref = "refs/caos/conversations/first/from-user";
        let second_ref = "refs/caos/conversations/second/from-user";

        publish_user_conversation(&transport, "alice", "first", "First title").unwrap();
        publish_user_conversation(&transport, "alice", "second", "Second title").unwrap();
        publish_user_conversation(&transport, "bob", "first", "First title").unwrap();
        let first_head = git(&repo, &["rev-parse", first_ref]);
        let tree = git(&repo, &["rev-parse", &format!("{first_head}^{{tree}}")]);
        let divergent = git(&repo, &["commit-tree", &tree, "-m", "divergent"]);
        git(&repo, &["update-ref", first_ref, &divergent]);
        assert!(
            publish_user_conversation(&transport, "alice", "first", "Conflict").is_err(),
            "a non-fast-forward conversation HEAD was accepted"
        );
        git(&repo, &["update-ref", first_ref, &first_head]);
        git(&repo, &["update-ref", "-d", first_ref]);
        git(&repo, &["update-ref", "-d", second_ref]);
        let active =
            list_user_conversations(&transport, "alice", UserConversationStatus::Active).unwrap();
        assert_eq!(
            active
                .iter()
                .map(|conversation| (conversation.id.as_str(), conversation.title.as_str()))
                .collect::<Vec<_>>(),
            [("second", "Second title"), ("first", "First title")]
        );
        assert_eq!(
            git(&repo, &["rev-parse", first_ref]),
            active
                .iter()
                .find(|conversation| conversation.id == "first")
                .unwrap()
                .head
        );

        set_conversation_title(&transport, "first", "Renamed").unwrap();
        archive_user_conversation(&transport, "alice", "first").unwrap();
        assert_eq!(
            list_user_conversations(&transport, "alice", UserConversationStatus::Active)
                .unwrap()
                .iter()
                .map(|conversation| conversation.id.as_str())
                .collect::<Vec<_>>(),
            ["second"]
        );
        let archived =
            list_user_conversations(&transport, "alice", UserConversationStatus::Archived).unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].id, "first");
        assert_eq!(archived[0].title, "Renamed");
        assert_eq!(
            list_user_conversations(&transport, "bob", UserConversationStatus::Active)
                .unwrap()
                .iter()
                .map(|conversation| conversation.id.as_str())
                .collect::<Vec<_>>(),
            ["first"]
        );

        unarchive_user_conversation(&transport, "alice", "first").unwrap();
        assert_eq!(
            list_user_conversations(&transport, "alice", UserConversationStatus::Active)
                .unwrap()
                .iter()
                .map(|conversation| conversation.id.as_str())
                .collect::<Vec<_>>(),
            ["second", "first"]
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_bare_conversation_refs_migrate_to_from_user() {
        let root = std::env::temp_dir().join(format!(
            "caos-legacy-migrate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet"]);
        git(&repo, &["config", "user.name", "tester"]);
        git(&repo, &["config", "user.email", "tester@example.com"]);
        std::fs::write(repo.join("file"), "one\n").unwrap();
        git(&repo, &["add", "file"]);
        git(&repo, &["commit", "--quiet", "-m", "one"]);
        let c = git(&repo, &["rev-parse", "HEAD"]);

        // Two legacy bare heads (one slashed) and one already-migrated head.
        git(
            &repo,
            &["update-ref", "refs/caos/conversations/old-one", &c],
        );
        git(
            &repo,
            &["update-ref", "refs/caos/conversations/proj/feature", &c],
        );
        git(
            &repo,
            &["update-ref", "refs/caos/conversations/kept/from-user", &c],
        );

        let t = GitTransport::discover(&repo).unwrap();
        migrate_legacy_conversation_refs(&t).unwrap();
        let resolve = |name: &str| rev_parse_opt(&t, name).unwrap();

        // Bare heads gone; their `from-user` channels now hold the commit.
        assert!(resolve("refs/caos/conversations/old-one").is_none());
        assert_eq!(
            resolve("refs/caos/conversations/old-one/from-user").as_deref(),
            Some(c.as_str())
        );
        assert!(resolve("refs/caos/conversations/proj/feature").is_none());
        assert_eq!(
            resolve("refs/caos/conversations/proj/feature/from-user").as_deref(),
            Some(c.as_str())
        );
        // The already-migrated head is untouched.
        assert_eq!(
            resolve("refs/caos/conversations/kept/from-user").as_deref(),
            Some(c.as_str())
        );

        // Idempotent, and list_conversations now sees all three.
        migrate_legacy_conversation_refs(&t).unwrap();
        let mut names: Vec<String> = list_conversations(&t)
            .unwrap()
            .into_iter()
            .map(|conversation| conversation.name)
            .collect();
        names.sort();
        assert_eq!(names, ["kept", "old-one", "proj/feature"]);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_server_conversation_head_migrates_on_list() {
        let (root, repo) = conversation_repo();
        let transport = GitTransport::discover(&repo).unwrap();
        let first_ref = "refs/caos/conversations/first/from-user";
        let from_user = "refs/caos/conversations/legacy/from-user";
        let legacy_head = "refs/caos/conversations/legacy/head";

        // A conversation an older TUI published lives only at <id>/head on the
        // server, indexed active for the user — no <id>/from-user yet.
        let head = git(&repo, &["rev-parse", first_ref]);
        let title = transport
            .put_object("blob", b"Legacy title")
            .unwrap()
            .to_string();
        transport.ensure_pushed(&title).unwrap();
        git(
            &repo,
            &[
                "push",
                CAOS_REMOTE,
                &format!("{head}:{legacy_head}"),
                &format!("{title}:refs/caos/conversations/legacy/title"),
                &format!("{head}:refs/caos/users/tester/conversations/active/legacy"),
            ],
        );

        let listed =
            list_user_conversations(&transport, "tester", UserConversationStatus::Active).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "legacy");
        assert_eq!(listed[0].title, "Legacy title");
        assert_eq!(listed[0].head, head);

        // The server head was renamed to from-user and the local ref set.
        let patterns = [from_user.to_string(), legacy_head.to_string()];
        let server = remote_refs(&transport, patterns).unwrap();
        assert_eq!(
            server.get(from_user).map(String::as_str),
            Some(head.as_str())
        );
        assert!(!server.contains_key(legacy_head));
        assert_eq!(
            rev_parse_opt(&transport, from_user).unwrap().as_deref(),
            Some(head.as_str())
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
