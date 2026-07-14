//! `caos-cli chat` — the user-facing conversation client (see
//! design/agent-harness.md, "Client").
//!
//! One invocation is one turn: mint the human-turn commit (parent = the
//! conversation head, or the base for a new conversation; tree = the parent's
//! tree — human turns are text-only for now), hand it to an `llm-step` run,
//! watch the turn's progress ref while the run blocks, and on success advance
//! `refs/caos/conversations/<name>` to the returned turn commit. Conversation
//! identity is that ref — the only mutable thing, owned by this client. On a
//! failed run the ref is untouched; the minted human commit is harmlessly
//! orphaned.
//!
//! The worker binaries (`worker-llm-step`, `worker-bash-tool`) are static
//! binaries curried onto the shared `/cas/std/runner` pool image, exactly as
//! the llm-step tests do; `chat` needs their paths (git-tracked, like every
//! ingested path) via `--llm-step-bin`/`--bash-tool-bin` or the corresponding
//! env vars.

use std::collections::HashSet;
use std::io::{IsTerminal, Read};

use serde_json::Value;

use super::{
    curry_object, fetch_object, git_capture, prepare_request, request_compute, resolve_cli_image,
    GitTransport, HttpTransport, Transport, CAOS_REMOTE,
};

/// Author name on agent step/turn commits (see design/agent-harness.md): the
/// marker the conversation walk keys on, and therefore *reserved* — a human
/// turn must carry any other author.
const AGENT_AUTHOR: &str = "caos-agent";

/// The client-owned conversation head ref, in the *local* repo.
const CONV_REF_PREFIX: &str = "refs/caos/conversations/";

/// The server-side per-step progress ref the worker pushes.
const PROGRESS_REF_PREFIX: &str = "refs/caos/progress/";

/// The LLM API key rides in from the environment, never a flag (it would land
/// in shell history and process listings).
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// Env fallbacks for the worker-binary paths.
const LLM_STEP_BIN_ENV: &str = "CAOS_LLM_STEP_BIN";
const BASH_TOOL_BIN_ENV: &str = "CAOS_BASH_TOOL_BIN";

/// The std builtin the worker binaries run under (`curry(runner, bin=...)`).
const RUNNER_IMAGE: &str = "/cas/std/runner";

/// Default system prompt when neither `--system` nor `--system-file` is given.
const DEFAULT_SYSTEM: &str = "You are a coding agent operating on a git workspace. Use the bash \
     tool to inspect and change files, declaring every path a command reads in `paths`. Keep \
     responses concise.";

/// Seconds between progress-ref polls while the run blocks.
const POLL_SECS: u64 = 2;

/// Parsed `chat` arguments (see [`chat_usage`]).
struct ChatArgs {
    name: String,
    message: Option<String>,
    base: Option<String>,
    system: Option<String>,
    system_file: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    llm_step_bin: Option<String>,
    bash_tool_bin: Option<String>,
    log: bool,
}

fn chat_usage() -> String {
    "usage: chat <name> [-m <message>] [--base <revspec>] \
     [--system <text> | --system-file <path>] [--model <model>] [--base-url <url>] \
     [--llm-step-bin <path>] [--bash-tool-bin <path>] [--log]\n\
     One turn per invocation; the message is read from stdin without -m. \
     --log prints the conversation so far and runs nothing."
        .to_string()
}

impl ChatArgs {
    fn parse(args: &[String]) -> Result<ChatArgs, String> {
        let mut it = args.iter();
        let mut a = ChatArgs {
            name: String::new(),
            message: None,
            base: None,
            system: None,
            system_file: None,
            model: None,
            base_url: None,
            llm_step_bin: None,
            bash_tool_bin: None,
            log: false,
        };
        let mut name: Option<String> = None;
        while let Some(arg) = it.next() {
            let mut value = |flag: &str| {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value\n{}", chat_usage()))
            };
            match arg.as_str() {
                "-m" | "--message" => a.message = Some(value(arg)?),
                "--base" => a.base = Some(value(arg)?),
                "--system" => a.system = Some(value(arg)?),
                "--system-file" => a.system_file = Some(value(arg)?),
                "--model" => a.model = Some(value(arg)?),
                "--base-url" => a.base_url = Some(value(arg)?),
                "--llm-step-bin" => a.llm_step_bin = Some(value(arg)?),
                "--bash-tool-bin" => a.bash_tool_bin = Some(value(arg)?),
                "--log" => a.log = true,
                other if other.starts_with('-') => {
                    return Err(format!("unknown chat option {other}\n{}", chat_usage()))
                }
                _ if name.is_none() => name = Some(arg.clone()),
                other => {
                    return Err(format!(
                        "chat takes one <name>, got an extra: {other}\n{}",
                        chat_usage()
                    ))
                }
            }
        }
        a.name = name.ok_or_else(chat_usage)?;
        if a.system.is_some() && a.system_file.is_some() {
            return Err("--system and --system-file are mutually exclusive".to_string());
        }
        Ok(a)
    }
}

/// `chat <name> …` — see the module docs and [`chat_usage`].
pub fn cli_chat(t: &GitTransport, args: &[String]) -> Result<(), String> {
    let a = ChatArgs::parse(args)?;
    let refname = format!("{CONV_REF_PREFIX}{}", a.name);
    // The name becomes two ref components (conversation + progress), so let git
    // validate it up front.
    git_capture(&["check-ref-format", &refname], None)
        .map_err(|_| format!("invalid conversation name {:?}", a.name))?;
    if a.log {
        return print_log(&a.name, &refname);
    }
    turn(t, &a, &refname)
}

/// One turn: mint the human commit, run llm-step over it, stream progress,
/// advance the conversation ref.
fn turn(t: &GitTransport, a: &ChatArgs, refname: &str) -> Result<(), String> {
    // Everything that can fail cheaply fails *before* the human commit is
    // minted or anything is pushed.
    let api_key = std::env::var(API_KEY_ENV).map_err(|_| {
        format!("{API_KEY_ENV} must be set (it rides, curried, into the llm-step run)")
    })?;
    let llm_bin = worker_bin(a.llm_step_bin.as_deref(), LLM_STEP_BIN_ENV, "--llm-step-bin")?;
    let bash_bin = worker_bin(a.bash_tool_bin.as_deref(), BASH_TOOL_BIN_ENV, "--bash-tool-bin")?;
    let system = match (&a.system, &a.system_file) {
        (Some(text), _) => text.clone(),
        (None, Some(path)) => {
            std::fs::read_to_string(path).map_err(|e| format!("--system-file {path}: {e}"))?
        }
        (None, None) => DEFAULT_SYSTEM.to_string(),
    };

    // The human commit's parent: the conversation head, or — for a new
    // conversation — the base commit (HEAD unless --base overrides).
    let parent = match rev_parse_opt(refname)? {
        Some(head) => head,
        None => {
            let rev = a.base.as_deref().unwrap_or("HEAD");
            let base = t
                .resolve_revspec(rev)?
                .ok_or_else(|| format!("cannot resolve --base {rev:?}"))?
                .to_string();
            // `.caos` is the harness's reserved top-level workspace entry
            // (step transcripts live there): refuse to start a conversation
            // over a tree that already carries one.
            if rev_parse_opt(&format!("{base}:.caos"))?.is_some() {
                return Err("the base commit's tree contains a top-level `.caos` entry, which \
                     is reserved for the agent harness; start from a tree without one"
                    .to_string());
            }
            base
        }
    };

    let message = read_message(a)?;
    // The agent author name is the turn-walk marker; a human commit carrying it
    // would corrupt every future transcript walk.
    let ident = git_capture(&["var", "GIT_AUTHOR_IDENT"], None)
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
    let tree = git_capture(&["rev-parse", &format!("{parent}^{{tree}}")], None)?
        .trim()
        .to_string();
    let human = git_capture(&["commit-tree", &tree, "-p", &parent, "-m", &message], None)?
        .trim()
        .to_string();

    // The workers: static binaries curried onto the shared runner-pool image
    // (exactly how tests/llm-step builds them). The bash curry's hash is passed
    // to llm-step as a *literal* (an image ref string), so its closure doesn't
    // ride in the request graph — push it (and the runner image) explicitly.
    let runner = resolve_cli_image(t, RUNNER_IMAGE)?;
    let bash_image = curry_object(t, &runner, None, &[format!("--bin:@={bash_bin}")])?.to_string();
    t.ensure_pushed(&bash_image)?;
    t.ensure_pushed(&runner)?;

    let mut kvs = vec![
        format!("--bin:@={llm_bin}"),
        format!("--api_key={api_key}"),
        format!("--system={system}"),
        format!("--bash_image={bash_image}"),
        format!("--conversation={}", a.name),
    ];
    if let Some(model) = &a.model {
        kvs.push(format!("--model={model}"));
    }
    if let Some(url) = &a.base_url {
        kvs.push(format!("--base_url={url}"));
    }
    let llm = curry_object(t, &runner, None, &kvs)?.to_string();

    // Build + push the request (this also pushes the human commit's closure —
    // the `:commit=` machinery), then trigger the blocking compute on its own
    // thread: request_compute needs only two strings, so the transport (and
    // the repo handle) stay on this thread for progress polling.
    let req = prepare_request(t, &llm, None, &[format!("--head:commit={human}")])?;
    let server = t.server_url()?;
    let run = {
        let (server, req) = (server.clone(), req);
        std::thread::spawn(move || request_compute(&server, &req))
    };

    // While the run blocks, follow the worker's per-step progress ref and
    // print each new step (assistant text + one-line tool calls).
    let http = HttpTransport { base: server };
    let progress_ref = format!("{PROGRESS_REF_PREFIX}{}", a.name);
    let mut printed: HashSet<String> = HashSet::new();
    while !run.is_finished() {
        for _ in 0..(POLL_SECS * 10) {
            if run.is_finished() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if run.is_finished() {
            break;
        }
        if let Err(e) = poll_progress(&http, &progress_ref, &human, &mut printed) {
            eprintln!("chat: progress poll failed (non-fatal): {e}");
        }
    }

    let outcome = run.join().map_err(|_| "the run thread panicked".to_string())?;
    let (kind, turn_hash) = match outcome {
        Ok(result) => result,
        Err(e) => {
            // Show whatever steps did land before the failure, then fail; the
            // conversation ref is untouched (the human commit is harmlessly
            // orphaned — see design/agent-harness.md).
            let _ = poll_progress(&http, &progress_ref, &human, &mut printed);
            return Err(format!(
                "turn failed; {refname} was not advanced.\n{e}"
            ));
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
    fetch_object(&turn_hash)?;
    let mut show_message = true;
    if let Some(tail) = rev_parse_opt(&format!("{turn_hash}^2"))? {
        if printed.contains(&tail) {
            show_message = false;
        } else {
            let _ = drain_steps(&http, &tail, &human, &mut printed, Some(&tail));
        }
    }

    git_capture(&["update-ref", refname, &turn_hash], None)?;
    let text = git_capture(&["show", "-s", "--format=%B", &turn_hash], None)?;
    let short = git_capture(&["rev-parse", "--short", &turn_hash], None)?
        .trim()
        .to_string();
    if show_message {
        println!("{}", text.trim_end());
    }
    println!("[{} {}]", a.name, short);
    Ok(())
}

/// A worker binary's path: the flag, else its env var, else a pointed error.
fn worker_bin(flag_value: Option<&str>, env: &str, flag: &str) -> Result<String, String> {
    if let Some(p) = flag_value {
        return Ok(p.to_string());
    }
    std::env::var(env).map_err(|_| {
        format!(
            "chat needs the {} worker binary: pass {flag} <path> or set {env} \
             (a git-tracked path; build it with `nix build .#{}`)",
            flag.trim_start_matches("--").trim_end_matches("-bin"),
            if flag == "--llm-step-bin" {
                "worker-llm-step"
            } else {
                "worker-bash-tool"
            }
        )
    })
}

/// The turn's message: `-m`, or stdin read to EOF.
fn read_message(a: &ChatArgs) -> Result<String, String> {
    let raw = match &a.message {
        Some(m) => m.clone(),
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
fn rev_parse_opt(spec: &str) -> Result<Option<String>, String> {
    match git_capture(&["rev-parse", "--verify", "--quiet", spec], None) {
        Ok(out) => Ok(Some(out.trim().to_string())),
        Err(_) => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Progress: follow refs/caos/progress/<name> while the run blocks.
// ---------------------------------------------------------------------------

/// One poll: read the progress ref off the server and print any new steps.
/// The ref not existing yet (first round still in flight) is normal.
fn poll_progress(
    http: &HttpTransport,
    progress_ref: &str,
    human: &str,
    printed: &mut HashSet<String>,
) -> Result<(), String> {
    let out = git_capture(&["ls-remote", CAOS_REMOTE, progress_ref], None)?;
    let Some(tip) = out.split_whitespace().next().filter(|h| !h.is_empty()) else {
        return Ok(()); // no ref yet
    };
    drain_steps(http, tip, human, printed, None)
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
        print_step(&step, suppress_text == Some(hash.as_str()));
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

/// Print one step: its assistant text blocks (unless suppressed) and a `$ cmd`
/// line per tool call. Thinking blocks stay private.
fn print_step(step: &Value, suppress_text: bool) {
    let Some(blocks) = step["content"].as_array() else {
        return;
    };
    for block in blocks {
        match block["type"].as_str() {
            Some("text") if !suppress_text => {
                let text = block["text"].as_str().unwrap_or("").trim_end();
                if !text.is_empty() {
                    println!("{text}");
                }
            }
            Some("tool_use") => {
                if block["name"] == "bash" {
                    println!("$ {}", block["input"]["cmd"].as_str().unwrap_or("?"));
                } else {
                    println!("[tool call: {}]", block["name"].as_str().unwrap_or("?"));
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// --log: the conversation so far, from the local ref with plain git.
// ---------------------------------------------------------------------------

/// Print the conversation's turns oldest-first: a first-parent walk down from
/// the head. Below a turn commit sits its human turn; below a human turn,
/// either the previous (agent-authored) turn or the base commit — which ends
/// the conversation (design/agent-harness.md, "Commit structure").
fn print_log(name: &str, refname: &str) -> Result<(), String> {
    let head = rev_parse_opt(refname)?
        .ok_or_else(|| format!("no conversation {name:?} ({refname} not found)"))?;
    let mut turns = Vec::new();
    let mut cur = head;
    let mut prev_was_agent = false;
    loop {
        let author = git_capture(&["show", "-s", "--format=%an", &cur], None)?
            .trim()
            .to_string();
        let is_agent = author == AGENT_AUTHOR;
        if !is_agent && !prev_was_agent {
            break; // the base commit — the conversation starts above it
        }
        let short = git_capture(&["rev-parse", "--short", &cur], None)?
            .trim()
            .to_string();
        let message = git_capture(&["show", "-s", "--format=%B", &cur], None)?
            .trim_end()
            .to_string();
        turns.push((short, author, message));
        let Some(parent) = rev_parse_opt(&format!("{cur}^"))? else {
            break;
        };
        prev_was_agent = is_agent;
        cur = parent;
    }
    turns.reverse();
    for (short, author, message) in turns {
        println!("── {short} {author}");
        println!("{message}");
        println!();
    }
    Ok(())
}
