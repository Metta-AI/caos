//! `caos-cli talk` / `chat` — the user-facing conversation client (see
//! design/agent-harness.md, "Client").
//!
//! One turn: mint the human-turn commit (parent = the conversation head, or
//! the base for a new conversation; tree = the parent's tree — human turns
//! are text-only for now), hand it to an `llm-step` run, watch the turn's
//! progress ref while the run blocks, and on success advance
//! `refs/caos/conversations/<name>` to the returned turn commit. Conversation
//! identity is that ref — the only mutable thing, owned by this client. On a
//! failed run the ref is untouched; the minted human commit is harmlessly
//! orphaned.
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

use std::collections::HashSet;
use std::io::{IsTerminal, Read};

use serde_json::Value;

use super::{
    curry_object, prepare_request, request_compute, resolve_cli_image, GitTransport, HttpTransport,
    Transport, CAOS_REMOTE,
};

/// Author name on agent step/turn commits (see design/agent-harness.md): the
/// marker the conversation walk keys on, and therefore *reserved* — a human
/// turn must carry any other author.
const AGENT_AUTHOR: &str = "caos-agent";

/// The client-owned conversation head ref, in the *local* repo.
const CONV_REF_PREFIX: &str = "refs/caos/conversations/";

/// A conversation's channels all live together under [`CONV_REF_PREFIX`]:
/// `<name>` (the head, local, client-owned) plus two server-side refs the
/// worker pushes — `<name>-progress` (the growing step chain) and
/// `<name>-status` (a blob `"<human hash>\n<text>"` force-updated around each
/// API attempt: calling / retrying / answered-in; the hash scopes it to a
/// turn, so a stale one is ignorable). The suffixes are reserved in
/// [`validated_refname`] so a conversation can't shadow another's channels.
const PROGRESS_SUFFIX: &str = "-progress";
const STATUS_SUFFIX: &str = "-status";

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

/// Auto-named conversations (`talk` with no `-c`): `talk-1`, `talk-2`, …
const AUTO_NAME_PREFIX: &str = "talk-";

/// Default system prompt when neither `--system` nor `--system-file` is given.
const DEFAULT_SYSTEM: &str = "You are a coding agent operating on a git workspace. Use the \
     read/ls/write/edit tools for file access and grep to search. Use the bash tool to run \
     commands (builds, tests, scripts), declaring every path a command reads in `paths`. Keep \
     responses concise.";

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

/// Structured progress from one turn. Frontends decide how to render these;
/// the harness never needs to know whether its caller is a pipe, a terminal,
/// or a full-screen UI.
#[derive(Clone, Debug, PartialEq)]
pub enum TurnEvent {
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

/// The accumulated workspace change carried by a conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceDiff {
    pub base: String,
    pub head: String,
    pub stat: String,
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

/// `chat <name> …` — the explicit, scriptable one-turn form; see [`usage`].
pub fn cli_chat(t: &GitTransport, args: &[String]) -> Result<(), String> {
    let a = ChatArgs::parse(Verb::Chat, args)?;
    let name = a.name.clone().expect("chat parse requires a name");
    let refname = validated_refname(t, &name)?;
    if a.log {
        return print_log(t, &name, &refname);
    }
    let message = read_message(a.message.as_deref())?;
    run_cli_turn(t, &a.turn_options(), &name, &message)
}

/// `talk [<prompt>] …` — the everyday surface; see [`usage`] and module docs.
pub fn cli_talk(t: &GitTransport, args: &[String]) -> Result<(), String> {
    let a = ChatArgs::parse(Verb::Talk, args)?;
    let (name, fresh) = pick_conversation(t, &a)?;
    let refname = validated_refname(t, &name)?;
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
    run_chat_turn(t, options, name, message, |event| match event {
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
        TurnEvent::PhaseComplete { .. } | TurnEvent::ToolResult { .. } => {}
    })?;
    Ok(())
}

/// The conversation ref for `name`, validated up front (the name also becomes
/// the `-progress`/`-status` channel refs, so let git check it — and reserve
/// those suffixes, or conversation `foo-progress` would shadow `foo`'s
/// channel).
fn validated_refname(t: &GitTransport, name: &str) -> Result<String, String> {
    for suffix in [PROGRESS_SUFFIX, STATUS_SUFFIX] {
        if name.ends_with(suffix) {
            return Err(format!(
                "conversation names ending in {suffix:?} are reserved (channel refs)"
            ));
        }
    }
    let refname = format!("{CONV_REF_PREFIX}{name}");
    t.git_capture(&["check-ref-format", &refname], None)
        .map_err(|_| format!("invalid conversation name {name:?}"))?;
    Ok(refname)
}

/// Which conversation a `talk` invocation is about, and whether it's new:
/// `-c <name>` names one (existing or not); `--new` mints a fresh auto-named
/// one; with neither, the repo's most recently advanced conversation — or a
/// fresh one when there is none yet.
fn pick_conversation(t: &GitTransport, a: &ChatArgs) -> Result<(String, bool), String> {
    if let Some(name) = &a.name {
        if a.new_conv && rev_parse_opt(t, &format!("{CONV_REF_PREFIX}{name}"))?.is_some() {
            return Err(format!(
                "--new: conversation {name:?} already exists (drop --new to continue it)"
            ));
        }
        let fresh = rev_parse_opt(t, &format!("{CONV_REF_PREFIX}{name}"))?.is_none();
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
/// committer date (turn commits carry wall-clock timestamps). Channel refs
/// (`-progress`/`-status`) are server-side, but skip them defensively in case
/// a broad fetch ever mirrored them here.
fn latest_conversation(t: &GitTransport) -> Result<Option<String>, String> {
    Ok(list_conversations(t)?.into_iter().next().map(|c| c.name))
}

/// List the local conversation refs, newest first. Progress/status channel refs
/// are server-owned implementation details and never appear in this list.
pub fn list_conversations(t: &GitTransport) -> Result<Vec<ConversationSummary>, String> {
    let out = t.git_capture(
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)%09%(objectname)%09%(committerdate:unix)",
            CONV_REF_PREFIX.trim_end_matches('/'),
        ],
        None,
    )?;
    let mut conversations = Vec::new();
    for line in out.lines() {
        let mut fields = line.split('\t');
        let Some(refname) = fields.next() else {
            continue;
        };
        let Some(name) = refname.strip_prefix(CONV_REF_PREFIX) else {
            continue;
        };
        if name.ends_with(PROGRESS_SUFFIX) || name.ends_with(STATUS_SUFFIX) {
            continue;
        }
        let head = fields.next().unwrap_or_default().to_string();
        let updated_unix = fields
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or_default();
        conversations.push(ConversationSummary {
            name: name.to_string(),
            head,
            updated_unix,
        });
    }
    Ok(conversations)
}

/// Read a named conversation's clean human/agent spine, oldest first.
pub fn conversation_history(t: &GitTransport, name: &str) -> Result<Vec<ConversationTurn>, String> {
    let refname = validated_refname(t, name)?;
    let head = rev_parse_opt(t, &refname)?
        .ok_or_else(|| format!("no conversation {name:?} ({refname} not found)"))?;
    history_from_head(t, &head).map(|(turns, _base)| turns)
}

/// Diff the conversation's current workspace against the commit it started
/// from. This operation is side-effect free; clients own any policy for
/// applying or publishing the returned change.
pub fn conversation_workspace_diff(t: &GitTransport, name: &str) -> Result<WorkspaceDiff, String> {
    let refname = validated_refname(t, name)?;
    let head = rev_parse_opt(t, &refname)?
        .ok_or_else(|| format!("no conversation {name:?} ({refname} not found)"))?;
    let (_turns, base) = history_from_head(t, &head)?;
    let stat = t.git_capture(
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--stat",
            &base,
            &head,
        ],
        None,
    )?;
    let patch = t.git_capture(&["diff", "--no-ext-diff", "--no-color", &base, &head], None)?;
    Ok(WorkspaceDiff {
        base,
        head,
        stat,
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
    mut emit: impl FnMut(TurnEvent),
) -> Result<TurnOutcome, String> {
    let refname = validated_refname(t, name)?;
    if message.trim().is_empty() {
        return Err("empty message".to_string());
    }
    turn(t, options, name, &refname, message.trim(), &mut emit)
}

/// One turn: mint the human commit, run llm-step over it, emit progress, and
/// advance the conversation ref.
fn turn(
    t: &GitTransport,
    options: &TurnOptions,
    name: &str,
    refname: &str,
    message: &str,
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
    let ident = t
        .git_capture(&["var", "GIT_AUTHOR_IDENT"], None)
        .map_err(|e| format!("no git author identity (set user.name/user.email): {e}"))?;
    if ident.split(" <").next().unwrap_or("").trim() == AGENT_AUTHOR {
        return Err(format!(
            "your git author name is {AGENT_AUTHOR:?}, which is reserved for agent commits; \
             set a different user.name"
        ));
    }

    // Mint the human turn: parent = head/base, tree = parent's tree (human
    // turns are text-only for now), message = the user's text, author = the
    // user's git identity.
    let tree = t
        .git_capture(&["rev-parse", &format!("{parent}^{{tree}}")], None)?
        .trim()
        .to_string();
    let human = t
        .git_capture(&["commit-tree", &tree, "-p", &parent, "-m", message], None)?
        .trim()
        .to_string();

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
            let img = curry_object(t, runner, None, &[format!("--bin:@={bin}")])?.to_string();
            t.ensure_pushed(&img)?;
            t.ensure_pushed(runner)?;
            img
        }
        None => resolve_cli_image(t, BASH_TOOL_IMAGE)?,
    };

    let grep_image = match &rgrep_bin {
        Some(bin) => {
            let runner = runner.as_deref().expect("resolved when a bin is given");
            let img = curry_object(t, runner, None, &[format!("--bin:@={bin}")])?.to_string();
            t.ensure_pushed(&img)?;
            t.ensure_pushed(runner)?;
            img
        }
        None => resolve_cli_image(t, RGREP_IMAGE)?,
    };

    let mut kvs = vec![
        format!("--api_key={api_key}"),
        format!("--system={system}"),
        format!("--bash_image={bash_image}"),
        format!("--grep_image={grep_image}"),
        format!("--conversation={name}"),
    ];
    if let Some(model) = &options.model {
        kvs.push(format!("--model={model}"));
    }
    if let Some(url) = &options.base_url {
        kvs.push(format!("--base_url={url}"));
    }
    // Per-turn state currying: onto the std llm-step curry (layers flatten, so
    // the result is exactly curry(runner, bin, <state>)), or onto the runner
    // with the override binary.
    let llm_base = match &llm_bin {
        Some(bin) => {
            kvs.push(format!("--bin:@={bin}"));
            runner.clone().expect("resolved when a bin is given")
        }
        None => resolve_cli_image(t, LLM_STEP_IMAGE)?,
    };
    let llm = curry_object(t, &llm_base, None, &kvs)?.to_string();
    emit(TurnEvent::PhaseComplete {
        label: "resolving the workers".to_string(),
        elapsed_secs: phase.elapsed().as_secs_f64(),
    });

    // Build + push the request (this also pushes the human commit's closure —
    // the `:commit=` machinery), then trigger the blocking compute on its own
    // thread: request_compute needs only two strings, so the transport (and
    // the repo handle) stay on this thread for progress polling.
    let phase = std::time::Instant::now();
    let req = prepare_request(t, &llm, None, &[format!("--head:commit={human}")])?;
    emit(TurnEvent::PhaseComplete {
        label: "pushing the turn".to_string(),
        elapsed_secs: phase.elapsed().as_secs_f64(),
    });
    let server = t.server_url()?;
    let run = {
        let (server, req) = (server.clone(), req);
        std::thread::spawn(move || request_compute(&server, &req))
    };

    // While the run blocks, follow the worker's per-step progress ref and
    // print each new step (assistant text + one-line tool calls); alongside
    // it, the in-round status ref — what the API call is doing right now —
    // goes to stderr (transient meta, not conversation content).
    let http = HttpTransport { base: server };
    let progress_ref = format!("{CONV_REF_PREFIX}{name}{PROGRESS_SUFFIX}");
    let status_ref = format!("{CONV_REF_PREFIX}{name}{STATUS_SUFFIX}");
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
    // Negotiating with the human commit as the tip keeps the pack to this
    // turn's new objects — a noop fetch would re-download the workspace
    // closure (including the base's whole history) every turn.
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

    t.git_capture(&["update-ref", refname, &turn_hash], None)?;
    let text = t.git_capture(&["show", "-s", "--format=%B", &turn_hash], None)?;
    let short = t
        .git_capture(&["rev-parse", "--short", &turn_hash], None)?
        .trim()
        .to_string();
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

/// `git rev-parse --verify --quiet <spec>`, `None` when it doesn't resolve.
fn rev_parse_opt(t: &GitTransport, spec: &str) -> Result<Option<String>, String> {
    match t.git_capture(&["rev-parse", "--verify", "--quiet", spec], None) {
        Ok(out) => Ok(Some(out.trim().to_string())),
        Err(_) => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Progress: follow the conversation's -progress ref while the run blocks.
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
    let out = t.git_capture(&["ls-remote", CAOS_REMOTE, progress_ref], None)?;
    let Some(tip) = out.split_whitespace().next().filter(|h| !h.is_empty()) else {
        return Ok(()); // no ref yet
    };
    drain_steps(http, tip, human, printed, None, emit)
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
    let out = t.git_capture(&["ls-remote", CAOS_REMOTE, status_ref], None)?;
    let Some(tip) = out.split_whitespace().next().filter(|h| !h.is_empty()) else {
        return Ok(()); // no ref yet
    };
    if last.as_deref() == Some(tip) {
        return Ok(());
    }
    let (kind, content) = http.get_object(tip)?;
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
    *last = Some(tip.to_string());
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
// --log: the conversation so far, from the local ref with plain git.
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
        let author = t
            .git_capture(&["show", "-s", "--format=%an", &cur], None)?
            .trim()
            .to_string();
        let is_agent = author == AGENT_AUTHOR;
        if !is_agent && !prev_was_agent {
            turns.reverse();
            return Ok((turns, cur)); // the base commit — conversation starts above it
        }
        let short = t
            .git_capture(&["rev-parse", "--short", &cur], None)?
            .trim()
            .to_string();
        let message = t
            .git_capture(&["show", "-s", "--format=%B", &cur], None)?
            .trim_end()
            .to_string();
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
        let Some(parent) = rev_parse_opt(t, &format!("{cur}^"))? else {
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
    use serde_json::json;

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
}
