//! caos-worker-llm-step: the agent-harness driver (see design/agent-harness.md).
//!
//! One invocation is one position in the step loop:
//!
//! * **Start** (`--head:commit=`, no `--result`): rebuild the conversation's
//!   API transcript from the commit chain, POST `/v1/messages`, and either
//!   mint the turn commit (no tool calls) or mint a step commit and launch the
//!   first tool call as a run-then sub-run, currying ourselves — with the step
//!   commit, the remaining pending calls, and the collected results — into
//!   `then`.
//! * **Callback** (`--result` present, from run-then): fold the tool's result
//!   into a `tool_result` block; if calls are still pending, launch the next
//!   one the same way; otherwise send all the results back in one user message
//!   (the next LLM round) and continue as above.
//!
//! Tool calls are driven serially through one queue (`drive`): the inline file
//! tools (read/ls/write/edit — `tools.rs`) execute in-process, advancing the
//! workspace with no sub-run; only `bash` exits into a run-then sub-run.
//!
//! Curried configuration: `api_key`, `system` (the system prompt), `bash_image`
//! (the sub-run tool's image), and optionally `model` (default
//! `claude-opus-4-8`), `base_url` (default `https://api.anthropic.com`;
//! overridable so tests can point it at a stub), and `conversation` (a name;
//! when present, each minted step pushes `refs/caos/conversations/<name>-progress`
//! and each API attempt updates `refs/caos/conversations/<name>-status` — see
//! `progress.rs`). Continuation state, curried by ourselves: `step` (the
//! current step commit), `pending` / `results` (JSON arrays of the remaining
//! `tool_use` blocks and the collected `tool_result` blocks), and
//! `current_id` (the in-flight call's `tool_use` id).
//!
//! Commit structure and the `.caos/step.json` format are documented in
//! design/agent-harness.md; the constants below are the load-bearing bits.

mod api;
mod progress;
mod tools;

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use worker_common::{
    arg, caos, caos_curry, cas_hash, entries, file_name, link, path, read_arg, read_arg_opt,
    read_commit, run_then, run_worker, scratch, write_commit_as, Arg, Commit,
};

/// Author name on step and turn commits — and how the conversation walk tells
/// an agent turn from the base commit below it.
const AGENT_AUTHOR: &str = "caos-agent";

/// The reserved top-level workspace entry holding a step's transcript.
const STEP_DIR: &str = ".caos";
const STEP_FILE: &str = "step.json";

fn main() -> std::process::ExitCode {
    run_worker("llm-step", run)
}

/// Curried configuration (see the module docs).
struct Config {
    api_key: String,
    system: String,
    bash_image: String,
    /// The rgrep fold worker's image; the `grep` tool is registered only when
    /// present (older curries without it keep working).
    grep_image: Option<String>,
    model: String,
    base_url: String,
    conversation: Option<String>,
}

impl Config {
    fn read() -> Result<Config, String> {
        Ok(Config {
            api_key: read_arg("api_key")?,
            system: read_arg("system")?,
            bash_image: read_arg("bash_image")?,
            grep_image: read_arg_opt("grep_image")?,
            model: read_arg_opt("model")?.unwrap_or_else(|| "claude-opus-4-8".to_string()),
            base_url: read_arg_opt("base_url")?
                .unwrap_or_else(|| "https://api.anthropic.com".to_string()),
            conversation: read_arg_opt("conversation")?,
        })
    }
}

/// Two positions, told apart by the args present: `--result` (run-then calling
/// us back with a tool's result) is the callback; otherwise this is the start
/// of a turn.
fn run() -> Result<(), String> {
    let cfg = Config::read()?;
    if Path::new(&arg("result")).exists() {
        callback(&cfg)
    } else {
        start(&cfg)
    }
}

/// Start of a turn: `head` is the human-turn commit to answer.
fn start(cfg: &Config) -> Result<(), String> {
    let head_hash = cas_hash(&arg("head"))?;
    // First signal of the turn: everything before it is client/dispatch, the
    // stretch from here to `calling <model>…` is transcript/workspace prep.
    progress::status(
        cfg.conversation.as_deref(),
        &head_hash,
        "preparing the turn…",
    );
    let head = read_commit(&arg("head"))?;
    let prior = prior_messages(&head)?;

    // The workspace this turn starts from: the head commit's tree.
    let ws = fresh("ws");
    caos(["get-hash", &head.tree, &ws])?;
    // `.caos` is reserved. At conversation start (no prior agent turns) the
    // base tree must not already carry one.
    if prior.is_empty() && Path::new(&ws).join(STEP_DIR).exists() {
        return Err(format!(
            "the conversation's base tree already contains the reserved {STEP_DIR:?} entry"
        ));
    }

    let mut messages = prior;
    messages.push(user_text(&head.message));
    llm_round(cfg, messages, &ws, &head_hash, &head_hash, &[])
}

/// Callback from run-then: `result` is the bash tool's result tree, `in` the
/// call it answered (unused — `current_id` carries the id), and the rest of
/// the loop state rode our own curry.
fn callback(cfg: &Config) -> Result<(), String> {
    let head_hash = cas_hash(&arg("head"))?;
    progress::status(
        cfg.conversation.as_deref(),
        &head_hash,
        "folding the tool result in…",
    );
    let pending = parse_blocks(&read_arg("pending")?, "pending")?;
    let mut results = parse_blocks(&read_arg("results")?, "results")?;
    let current_id = read_arg("current_id")?;

    // Fold the tool's outcome into a tool_result block the model will see,
    // and establish the workspace the queue continues over: bash results
    // carry the post-command workspace as `tree`; a grep result is a sparse
    // match tree, NOT a workspace — the pre-grep workspace rode our curry.
    let current_tool = read_arg_opt("current_tool")?.unwrap_or_else(|| "bash".to_string());
    let ws = match current_tool.as_str() {
        "grep" => {
            let scope = read_arg_opt("scope")?.unwrap_or_default();
            results.push(tools::grep_result_block(
                &current_id,
                &arg("result"),
                &scope,
            )?);
            let ws = arg("ws");
            caos(["get", &ws])?;
            ws
        }
        _ => {
            results.push(tool_result_block(&current_id)?);
            let ws = format!("{}/tree", arg("result"));
            if !Path::new(&ws).exists() {
                return Err("bash result carries no `tree` entry".to_string());
            }
            caos(["get", &ws])?;
            ws
        }
    };

    drive(cfg, ws, &head_hash, &arg("step"), &pending, results)
}

/// Work through the call queue: inline tools (read/ls/write/edit) execute
/// right here — the workspace advances in-process, no sub-run — while a bash
/// call exits into its run-then sub-run (the tail call; `callback` re-enters
/// this loop). A drained queue sends every result back in ONE user message
/// (the API requires it) and fires the next LLM round.
fn drive(
    cfg: &Config,
    mut ws: String,
    head_hash: &str,
    step_path: &str,
    queue: &[Value],
    mut results: Vec<Value>,
) -> Result<(), String> {
    let mut queue = queue.to_vec();
    while let Some(call) = queue.first().cloned() {
        let name = call["name"].as_str().unwrap_or("");
        if name == "bash" {
            return launch(cfg, &call, &ws, step_path, &queue[1..], &results);
        }
        if name == "grep" && cfg.grep_image.is_some() {
            // Validate before launching: a bad pattern or scope is an
            // is_error result and the queue continues — only a valid call
            // exits into the fold sub-run.
            match tools::grep_precheck(&call, &ws) {
                Err(block) => {
                    results.push(block);
                    queue.remove(0);
                    continue;
                }
                Ok((scope, prefix)) => {
                    return launch_grep(
                        cfg,
                        &call,
                        &scope,
                        &prefix,
                        &ws,
                        step_path,
                        &queue[1..],
                        &results,
                    )
                }
            }
        }
        if !tools::is_inline(name) {
            return Err(format!(
                "model called unknown tool {name:?} \
                 (registered: bash, grep, read, ls, write, edit)"
            ));
        }
        let (block, new_ws) = tools::execute(&call, &ws)?;
        results.push(block);
        if let Some(w) = new_ws {
            ws = w;
        }
        queue.remove(0);
    }

    // Queue drained: rebuild the transcript (prior turns + this turn's step
    // chain), append the results, next round.
    let step_hash = cas_hash(step_path)?;
    let head = read_commit(&arg("head"))?;
    let mut messages = prior_messages(&head)?;
    messages.push(user_text(&head.message));
    for step in step_chain(Some(&step_hash), head_hash)? {
        messages.extend(step_messages(&step));
    }
    messages.push(message("user", Value::Array(results.clone())));
    llm_round(cfg, messages, &ws, head_hash, &step_hash, &results)
}

/// One LLM API round over `messages`, with `ws` the workspace CAS path the
/// round is over, `prev` the commit the next step chains onto (the previous
/// step, or the human turn), and `sent_results` the tool_result blocks this
/// round's request carried (recorded in the step commit's step.json).
fn llm_round(
    cfg: &Config,
    messages: Vec<Value>,
    ws: &str,
    head_hash: &str,
    prev: &str,
    sent_results: &[Value],
) -> Result<(), String> {
    let body = json!({
        "model": cfg.model,
        "max_tokens": 16000,
        // Constrains model choice: adaptive thinking needs a 4.6+ model
        // (haiku-4-5 rejects it with a 400). Deliberately unconditional —
        // sniffing per-model capabilities here would rot.
        "thinking": {"type": "adaptive"},
        "cache_control": {"type": "ephemeral"},
        "system": cfg.system,
        "tools": registry(cfg),
        "messages": messages,
    });
    // Bracket the API call with status-ref updates (progress::status): the
    // call is the one silent, slow part of a turn, so say what it's doing —
    // and, via the retry callback, why it's waiting.
    let status = |text: &str| progress::status(cfg.conversation.as_deref(), head_hash, text);
    status(&format!("calling {}…", cfg.model));
    let started = std::time::Instant::now();
    let resp = api::post_messages(&cfg.base_url, &cfg.api_key, &body, &status)?;
    status(&format!(
        "{} answered in {:.1}s",
        cfg.model,
        started.elapsed().as_secs_f64()
    ));
    let stop = resp["stop_reason"].as_str().unwrap_or("").to_string();
    let blocks = resp["content"]
        .as_array()
        .cloned()
        .ok_or("API response has no content array")?;
    let tool_uses: Vec<Value> = blocks
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .cloned()
        .collect();

    match stop.as_str() {
        "end_turn" => {
            let text = response_text(&blocks);
            if prev == head_hash && sent_results.is_empty() {
                // No tool calls anywhere in this turn: no steps — the turn
                // commit's sole parent is the human turn, its tree unchanged.
                let tree = cas_hash(ws)?;
                write_commit_as(&tree, &[head_hash], &text, agent_now(), "/cas/out")?;
            } else {
                // The turn used tools: mint a final step (so this round's
                // blocks and the last tool results stay tree-reachable), then
                // the pure turn merge.
                let (step_hash, _) = mint_step(cfg, ws, prev, sent_results, &blocks)?;
                let tree = cas_hash(ws)?;
                write_commit_as(
                    &tree,
                    &[head_hash, &step_hash],
                    &text,
                    agent_now(),
                    "/cas/out",
                )?;
            }
            Ok(())
        }
        "tool_use" => {
            if tool_uses.is_empty() {
                return Err("stop_reason tool_use but no tool_use blocks".to_string());
            }
            let (_, step_path) = mint_step(cfg, ws, prev, sent_results, &blocks)?;
            drive(
                cfg,
                ws.to_string(),
                head_hash,
                &step_path,
                &tool_uses,
                Vec::new(),
            )
        }
        other => Err(format!(
            "LLM round ended with stop_reason {other:?} (only end_turn and tool_use \
             are handled; the turn fails here by design for now)"
        )),
    }
}

/// Mint a step commit: tree = the workspace plus `.caos/step.json` (this
/// round's verbatim response blocks and the tool_results its request carried),
/// parent = the previous step (or the human turn), author `caos-agent` at
/// wall-clock time. Pushes the progress ref (best-effort). Returns the
/// commit's `(hash, cas-path)`.
fn mint_step(
    cfg: &Config,
    ws: &str,
    parent: &str,
    sent_results: &[Value],
    blocks: &[Value],
) -> Result<(String, String), String> {
    let dir = scratch("steptree")?;
    for child in entries(ws)? {
        link(&child, dir.join(file_name(&child)))?;
    }
    fs::create_dir(dir.join(STEP_DIR)).map_err(|e| format!("creating {STEP_DIR}: {e}"))?;
    let step_json = json!({
        "content": blocks,
        "results": sent_results,
        "v": 1,
    });
    fs::write(dir.join(STEP_DIR).join(STEP_FILE), step_json.to_string())
        .map_err(|e| format!("writing {STEP_FILE}: {e}"))?;
    let tree_path = fresh("steptree");
    caos(["put", path(&dir), &tree_path])?;
    let tree_hash = cas_hash(&tree_path)?;

    let text = response_text(blocks);
    let message = if text.is_empty() {
        format!(
            "step: {} tool call(s)",
            blocks.iter().filter(|b| b["type"] == "tool_use").count()
        )
    } else {
        text
    };
    let commit_path = fresh("step");
    let hash = write_commit_as(&tree_hash, &[parent], &message, agent_now(), &commit_path)?;
    if let Some(conversation) = &cfg.conversation {
        progress::push(conversation, &hash);
    }
    Ok((hash, commit_path))
}

/// Launch one tool call as a run-then sub-run: `in` = `{tree, cmd, paths}`
/// (the workspace with `.caos` never present — `ws` is always a pure
/// workspace), `run` = the bash image, `then` = ourselves re-curried with the
/// loop state.
fn launch(
    cfg: &Config,
    call: &Value,
    ws: &str,
    step_path: &str,
    pending: &[Value],
    results: &[Value],
) -> Result<(), String> {
    let name = call["name"].as_str().unwrap_or("");
    if name != "bash" {
        return Err(format!(
            "launch got non-bash tool {name:?} (drive routes those inline)"
        ));
    }
    let id = call["id"]
        .as_str()
        .ok_or("tool_use block has no string id")?;
    let cmd = call["input"]["cmd"]
        .as_str()
        .ok_or("bash call has no string `cmd`")?;
    let paths: Vec<&str> = match &call["input"]["paths"] {
        Value::Null => Vec::new(),
        Value::Array(items) => items
            .iter()
            .map(|p| p.as_str().ok_or("bash call `paths` has a non-string entry"))
            .collect::<Result<_, _>>()?,
        _ => return Err("bash call `paths` is not an array".to_string()),
    };

    let dir = scratch("toolin")?;
    link(ws, dir.join("tree"))?;
    fs::write(dir.join("cmd"), cmd).map_err(|e| format!("writing cmd: {e}"))?;
    fs::write(dir.join("paths"), paths.join("\n")).map_err(|e| format!("writing paths: {e}"))?;
    let in_path = fresh("toolin");
    caos(["put", path(&dir), &in_path])?;

    let me = self_curry(
        step_path,
        pending,
        results,
        id,
        &[("current_tool", Arg::Lit("bash"))],
    )?;
    run_then(&in_path, &cfg.bash_image, Some(&me))
}

/// Launch a grep as a run-then sub-run of the rgrep fold worker: the input is
/// the scope subtree itself and the pattern rides curried on the image, so
/// every level of the fold caches on exactly (subtree hash, pattern). The
/// result is a sparse tree, not a workspace — the current `ws` rides the
/// continuation so the workspace is unchanged by a grep.
#[allow(clippy::too_many_arguments)]
fn launch_grep(
    cfg: &Config,
    call: &Value,
    scope: &str,
    scope_prefix: &str,
    ws: &str,
    step_path: &str,
    pending: &[Value],
    results: &[Value],
) -> Result<(), String> {
    let id = call["id"]
        .as_str()
        .ok_or("tool_use block has no string id")?;
    let pattern = call["input"]["pattern"]
        .as_str()
        .ok_or("grep call has no string `pattern` (precheck admits only those)")?;
    let image = cfg
        .grep_image
        .as_ref()
        .ok_or("launch_grep without a grep_image (drive guards this)")?;
    let curried = caos_curry(image, &[("pattern", Arg::Lit(pattern))])?;
    let me = self_curry(
        step_path,
        pending,
        results,
        id,
        &[
            ("current_tool", Arg::Lit("grep")),
            ("ws", Arg::Path(ws)),
            ("scope", Arg::Lit(scope_prefix)),
        ],
    )?;
    run_then(scope, &curried, Some(&me))
}

/// Rebuild ourselves as `curry(image, bin, <config>, <loop state>)` — a
/// source-built worker is curry(runner, bin) and gets unwrapped into args, so
/// "ourselves" is that same curry rebuilt from our own args (content-addressed,
/// hence identical), plus the state to remember. Commit-valued paths (`head`,
/// `step`) re-bind as gitlinks (their kind xattr rides the curry).
fn self_curry(
    step_path: &str,
    pending: &[Value],
    results: &[Value],
    current_id: &str,
    extras: &[(&str, Arg)],
) -> Result<String, String> {
    let pending_json = Value::Array(pending.to_vec()).to_string();
    let results_json = Value::Array(results.to_vec()).to_string();

    let bin = arg("bin");
    let head = arg("head");
    let api_key = arg("api_key");
    let system = arg("system");
    let bash_image = arg("bash_image");
    let mut kvs: Vec<(&str, Arg)> = vec![
        ("bin", Arg::Path(&bin)),
        ("head", Arg::Path(&head)),
        ("api_key", Arg::Path(&api_key)),
        ("system", Arg::Path(&system)),
        ("bash_image", Arg::Path(&bash_image)),
        ("step", Arg::Path(step_path)),
        ("pending", Arg::Lit(&pending_json)),
        ("results", Arg::Lit(&results_json)),
        ("current_id", Arg::Lit(current_id)),
    ];
    let optional: Vec<(&str, String)> = ["model", "base_url", "conversation", "grep_image"]
        .iter()
        .map(|name| (*name, arg(name)))
        .filter(|(_, p)| Path::new(p).exists())
        .collect();
    for (name, p) in &optional {
        kvs.push((name, Arg::Path(p)));
    }
    for (name, value) in extras {
        kvs.push((
            name,
            match value {
                Arg::Lit(s) => Arg::Lit(s),
                Arg::Path(s) => Arg::Path(s),
            },
        ));
    }
    caos_curry(&arg("image"), &kvs)
}

// ---------------------------------------------------------------------------
// Transcript reconstruction (see design/agent-harness.md, "Commit structure").
// ---------------------------------------------------------------------------

/// One step commit's `.caos/step.json` payload.
struct StepJson {
    /// The tool_result blocks this round's request carried (answers to the
    /// previous step's calls; empty for a turn's first round).
    results: Vec<Value>,
    /// The round's response content blocks, verbatim.
    content: Vec<Value>,
}

/// Messages for every completed turn strictly below `head` (oldest first) —
/// everything up to, but not including, head's own user message.
fn prior_messages(head: &Commit) -> Result<Vec<Value>, String> {
    // Walk the first-parent spine newest-first: below a human turn sits either
    // an agent turn merge (author caos-agent) or the conversation's base.
    let mut groups: Vec<Vec<Value>> = Vec::new();
    let mut parents = head.parents.clone();
    while let Some(parent) = parents.first().cloned() {
        let turn = fetch_commit(&parent)?;
        if turn.author != AGENT_AUTHOR {
            break; // the base commit — the conversation starts above it
        }
        let human_hash = turn
            .parents
            .first()
            .ok_or_else(|| format!("agent turn {parent} has no parents"))?
            .clone();
        let human = fetch_commit(&human_hash)?;
        let mut group = vec![user_text(&human.message)];
        group.extend(turn_messages(&turn, &human_hash)?);
        groups.push(group);
        parents = human.parents;
    }
    groups.reverse();
    Ok(groups.into_iter().flatten().collect())
}

/// Replay one completed agent turn: its steps' verbatim blocks — or, for a
/// turn that used no tools (and so has no steps), just its message text.
fn turn_messages(turn: &Commit, human_hash: &str) -> Result<Vec<Value>, String> {
    let steps = step_chain(turn.parents.get(1).map(String::as_str), human_hash)?;
    if steps.is_empty() {
        return Ok(vec![message(
            "assistant",
            Value::String(turn.message.clone()),
        )]);
    }
    Ok(steps.iter().flat_map(step_messages).collect())
}

/// A step's replayed messages: the tool_results its request carried (one user
/// message), then its assistant blocks, byte-exact.
fn step_messages(step: &StepJson) -> Vec<Value> {
    let mut msgs = Vec::new();
    if !step.results.is_empty() {
        msgs.push(message("user", Value::Array(step.results.clone())));
    }
    msgs.push(message("assistant", Value::Array(step.content.clone())));
    msgs
}

/// Walk a step chain from its tail commit back to `stop` (the human turn the
/// chain hangs off), returning the steps' payloads oldest-first.
fn step_chain(tail: Option<&str>, stop: &str) -> Result<Vec<StepJson>, String> {
    let mut steps = Vec::new();
    let mut cur = tail.map(str::to_string);
    while let Some(hash) = cur {
        if hash == stop {
            break;
        }
        let commit = fetch_commit(&hash)?;
        steps.push(read_step_json(&commit)?);
        cur = commit.parents.first().cloned();
    }
    steps.reverse();
    Ok(steps)
}

/// Fetch a commit by hash (materializing it at a fresh CAS path) and parse it.
fn fetch_commit(hash: &str) -> Result<Commit, String> {
    let p = fresh("commit");
    caos(["get-hash", hash, &p])?;
    read_commit(&p)
}

/// Read a step commit's `.caos/step.json`.
fn read_step_json(step: &Commit) -> Result<StepJson, String> {
    let tree = fresh("steptree-in");
    caos(["get-hash", &step.tree, &tree])?;
    let file = format!("{tree}/{STEP_DIR}/{STEP_FILE}");
    caos(["get", &format!("{tree}/{STEP_DIR}")])?;
    caos(["get", &file])?;
    let text = fs::read_to_string(&file).map_err(|e| format!("reading {file}: {e}"))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("parsing {STEP_FILE}: {e}"))?;
    let arr = |key: &str| -> Result<Vec<Value>, String> {
        v[key]
            .as_array()
            .cloned()
            .ok_or_else(|| format!("{STEP_FILE} has no {key} array"))
    };
    Ok(StepJson {
        results: arr("results")?,
        content: arr("content")?,
    })
}

// ---------------------------------------------------------------------------
// Blocks and small helpers.
// ---------------------------------------------------------------------------

/// The full tool registry: bash and grep (the sub-run tools) plus the inline
/// file tools (`tools.rs`).
fn registry(cfg: &Config) -> Vec<Value> {
    let mut tools = vec![bash_tool()];
    tools.extend(tools::declarations());
    if cfg.grep_image.is_some() {
        tools.push(tools::grep_declaration());
    }
    tools
}

/// The bash tool's registry entry, steering the model into the declared-paths
/// contract and the EACCES retry loop.
fn bash_tool() -> Value {
    json!({
        "name": "bash",
        "description": "Run a shell command in the workspace (executed with `sh -c` from the \
    workspace root). Use this for COMMANDS (builds, tests, scripts); for plain file access \
    prefer the read/ls/write/edit tools, which are immediate. The workspace is materialized \
    lazily: ONLY the files and directories you \
    list in `paths` are readable — a command touching any other existing path fails with \
    'Permission denied' (EACCES), and the result names the unmaterialized paths it touched. \
    When that happens, retry the same command with those paths added to `paths`. Creating new \
    files or directories needs no declaration. The result reports the exit code, stdout and \
    stderr (tails), and the workspace carries all changes forward. A non-zero exit is reported \
    back to you, not an error — read stderr and react.",
        "input_schema": {
            "type": "object",
            "properties": {
                "cmd": {
                    "type": "string",
                    "description": "The shell command to run."
                },
                "paths": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Workspace-relative paths the command reads or modifies; \
    only these are materialized into the sandbox."
                }
            },
            "required": ["cmd"]
        }
    })
}

/// The tool_result block for the bash result tree at `--result`: exit code,
/// stdout/stderr, the denied-paths retry hint when present — and `is_error`
/// on a non-zero exit, so the model treats it as a failure to react to.
fn tool_result_block(current_id: &str) -> Result<Value, String> {
    caos(["get", &arg("result")])?;
    let leaf = |name: &str| -> Result<String, String> {
        let p = format!("{}/{name}", arg("result"));
        caos(["get", &p])?;
        let bytes = fs::read(&p).map_err(|e| format!("reading {p}: {e}"))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    let exit = leaf("exit")?.trim().to_string();
    let stdout = leaf("stdout")?;
    let stderr = leaf("stderr")?;
    let denied = if Path::new(&format!("{}/denied", arg("result"))).exists() {
        Some(leaf("denied")?)
    } else {
        None
    };

    let mut text = format!("exit: {exit}\nstdout:\n{stdout}\nstderr:\n{stderr}");
    if let Some(denied) = &denied {
        text += &format!(
            "\nunmaterialized paths touched: {}; retry with them in `paths`.",
            denied.split_whitespace().collect::<Vec<_>>().join(", ")
        );
    }
    let mut block = json!({
        "type": "tool_result",
        "tool_use_id": current_id,
        "content": [{"type": "text", "text": text}],
    });
    if exit != "0" {
        block["is_error"] = Value::Bool(true);
    }
    Ok(block)
}

/// A `{role, content}` message.
fn message(role: &str, content: Value) -> Value {
    json!({"role": role, "content": content})
}

/// A user message holding plain text.
fn user_text(text: &str) -> Value {
    message("user", Value::String(text.trim_end().to_string()))
}

/// The concatenated text blocks of a response (the turn's message text).
fn response_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Author `caos-agent` at wall-clock now — step/turn commits carry real
/// timestamps, so a retried turn is a distinct commit.
fn agent_now() -> Option<(&'static str, i64)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some((AGENT_AUTHOR, now))
}

/// Parse a curried JSON array of blocks (`pending` / `results`).
fn parse_blocks(text: &str, what: &str) -> Result<Vec<Value>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("parsing {what}: {e}"))?;
    v.as_array()
        .cloned()
        .ok_or_else(|| format!("{what} is not a JSON array"))
}

/// A fresh, unique direct-child CAS path (CAS paths are single-assignment).
fn fresh(prefix: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("/cas/{prefix}-{n}")
}
