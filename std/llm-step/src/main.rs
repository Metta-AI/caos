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
//! Curried configuration: `api-key`, `system` (the system prompt), `bash-image`
//! (the sub-run tool's image), and optionally `model` (default
//! `claude-opus-4-8`), `base-url` (default `https://api.anthropic.com`;
//! overridable so tests can point it at a stub), and `conversation` (a name;
//! when present, each minted step pushes `refs/caos/conversations/<name>/from-agent`
//! and each API attempt updates `refs/caos/conversations/<name>/status` — see
//! `progress.rs`). Continuation state, curried by ourselves: `step` (the
//! current step commit), `pending` / `results` (JSON arrays of the remaining
//! `tool_use` blocks and the collected `tool_result` blocks), and
//! `current_id` (the in-flight call's `tool_use` id).
//!
//! Commit structure and the `.caos/step.json` format are documented in
//! design/agent-harness.md; the constants below are the load-bearing bits.

mod githist;
mod progress;
mod tools;

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use llm_client::{post_messages, DEFAULT_BASE_URL, DEFAULT_MODEL};
use serde_json::{json, Value};
use worker_common::{
    arg, caos, caos_curry, caos_recurry, cas_hash, entries, file_name, link, own_args_tree, path,
    read_arg, read_arg_opt, read_commit, run_then_catching, run_worker, scratch, write_commit_as,
    Arg, Commit,
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
    /// The script-worker image (std/bash) TREE TOOLS run on: the workspace's
    /// caos-tools/*.sh, discovered per round and resolved at invocation time
    /// (design/cargo-workers.md). Registered only when present.
    tools_image: Option<String>,
    /// The git-bearing merge worker (std/merge). The `merge` tool is registered
    /// only when present.
    merge_image: Option<String>,
    /// The turn-start ref snapshot: `name <hash>` lines the `merge` tool
    /// resolves `--theirs` against (SPEC "Resolving `--theirs`"). Absent = the
    /// tool takes only a bare hash.
    merge_refs: Option<String>,
    model: String,
    base_url: String,
    conversation: Option<String>,
}

/// A tool image bound by this entry's own `.caos-expr`. `--k=$VAR` in an
/// expression binds the object BY REFERENCE, so the arg materializes as a tree
/// (or a curry node) at `/cas/args/<name>` — its ref is the recorded hash, not
/// the file contents. Absent = the entry did not bind it.
fn image_arg(name: &str) -> Result<Option<String>, String> {
    let p = arg(name);
    if Path::new(&p).exists() {
        cas_hash(&p).map(Some)
    } else {
        Ok(None)
    }
}

impl Config {
    fn read() -> Result<Config, String> {
        Ok(Config {
            api_key: read_arg("api-key")?,
            system: read_arg("system")?,
            bash_image: image_arg("bash-image")?.ok_or("--bash-image is required")?,
            grep_image: image_arg("grep-image")?,
            tools_image: image_arg("tools-image")?,
            merge_image: image_arg("merge-image")?,
            merge_refs: read_arg_opt("merge-refs")?,
            model: read_arg_opt("model")?.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            base_url: read_arg_opt("base-url")?.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            conversation: read_arg_opt("conversation")?,
        })
    }
}

/// Two positions, told apart by the args present: `--result` (run-then calling
/// us back with a tool's result) or `--error` (calling us back with a tool's
/// FAILURE, via `run-then --catch`) is the callback; otherwise this is the
/// start of a turn.
fn run() -> Result<(), String> {
    let cfg = Config::read()?;
    if Path::new(&arg("result")).exists() || Path::new(&arg("error")).exists() {
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
    // The workspace commit threaded through the turn (SPEC "Tools thread a
    // commit"): starts as the head commit itself, advances as tools mutate.
    llm_round(
        cfg,
        messages,
        &ws,
        &arg("head"),
        &head_hash,
        &head_hash,
        &[],
    )
}

/// Callback from run-then: `result` is the sub-run tool's result, `in` the
/// call it answered (unused — `current_id` carries the id), and the rest of
/// the loop state rode our own curry. Establishes the workspace (`ws`) and the
/// workspace commit (`wc`) the queue continues over.
fn callback(cfg: &Config) -> Result<(), String> {
    let head_hash = cas_hash(&arg("head"))?;
    progress::status(
        cfg.conversation.as_deref(),
        &head_hash,
        "folding the tool result in…",
    );
    let pending = parse_blocks(&read_arg("pending")?, "pending")?;
    let mut results = parse_blocks(&read_arg("results")?, "results")?;
    let current_id = read_arg("current-id")?;

    // Fold the tool's outcome into a tool_result block the model will see,
    // and establish (ws, wc) the queue continues over.
    let current_tool = read_arg_opt("current-tool")?.unwrap_or_else(|| "bash".to_string());

    // The sub-run FAILED and `run-then --catch` handed us the failure instead of
    // killing the turn. There is no result to fold and no workspace to advance:
    // the pre-call `ws`/`wc` rode our own curry, so the queue continues from
    // exactly where it stood, and the model gets an is_error tool_result it can
    // read and react to — the whole point of catching (design/agent-harness.md,
    // "Tool failures are values, not errors"). Infrastructure failures used to
    // land here as a dead turn; now only the tool call is dead.
    if Path::new(&arg("error")).exists() {
        let text = read_arg("error")?;
        results.push(json!({
            "type": "tool_result",
            "tool_use_id": current_id,
            "is_error": true,
            "content": [{"type": "text", "text": format!(
                "the `{current_tool}` tool failed to run: {}\n\nThe workspace is unchanged. \
                 This is the tool itself failing, not a non-zero exit from your command.",
                text.trim_end()
            )}],
        }));
        let ws = arg("ws");
        caos(["get", &ws])?;
        return drive(
            cfg,
            ws,
            arg("wc"),
            &head_hash,
            &arg("step"),
            &pending,
            results,
        );
    }

    let (ws, wc) = match current_tool.as_str() {
        "grep" => {
            let scope = read_arg_opt("scope")?.unwrap_or_default();
            results.push(tools::grep_result_block(
                &current_id,
                &arg("result"),
                &scope,
            )?);
            let ws = arg("ws");
            caos(["get", &ws])?;
            // A grep is read-only: workspace and its commit are unchanged.
            (ws, arg("wc"))
        }
        // `merge` returns a COMMIT (its two-parent M): M becomes the workspace
        // commit, its tree the workspace, and the model hears about any
        // conflicts. Unlike every other tool, the ancestry advanced — that is
        // the whole point (SPEC "Merging and conflict resolution").
        "merge" => {
            let m = arg("result");
            let commit = read_commit(&m)?;
            let ws = fresh("ws");
            caos(["get-hash", &commit.tree, &ws])?;
            results.push(merge_result_block(&current_id, &ws)?);
            (ws, m)
        }
        // A tree tool's result (caos-tools/<name>.sh) is a VALUE — a report,
        // a bin tree, diagnostics — never a workspace: the pre-run workspace
        // and its commit rode our curry, exactly like grep.
        name if name != "bash" => {
            results.push(tools::tree_tool_result_block(&current_id, &arg("result"))?);
            let ws = arg("ws");
            caos(["get", &ws])?;
            (ws, arg("wc"))
        }
        _ => {
            results.push(tool_result_block(&current_id)?);
            let ws = format!("{}/tree", arg("result"));
            if !Path::new(&ws).exists() {
                return Err("bash result carries no `tree` entry".to_string());
            }
            caos(["get", &ws])?;
            // bash may have mutated the tree — advance the workspace commit.
            let wc = advance_wc(&ws, &arg("wc"), "bash")?;
            (ws, wc)
        }
    };

    drive(cfg, ws, wc, &head_hash, &arg("step"), &pending, results)
}

/// Work through the call queue, threading the workspace `ws` AND its commit
/// `wc` (SPEC "Tools thread a commit"): inline reads leave both unchanged;
/// inline mutations advance `ws` and mint a child `wc`; a bash/grep/tree/merge
/// call exits into its run-then sub-run (the tail call; `callback` re-enters).
/// A drained queue sends every result back in ONE user message and fires the
/// next LLM round.
fn drive(
    cfg: &Config,
    mut ws: String,
    mut wc: String,
    head_hash: &str,
    step_path: &str,
    queue: &[Value],
    mut results: Vec<Value>,
) -> Result<(), String> {
    let mut queue = queue.to_vec();
    while let Some(call) = queue.first().cloned() {
        let name = call["name"].as_str().unwrap_or("");
        if name == "bash" {
            return launch(cfg, &call, &ws, &wc, step_path, &queue[1..], &results);
        }
        if name == "merge" && cfg.merge_image.is_some() {
            match resolve_theirs(cfg, &call) {
                Err(block) => {
                    results.push(block);
                    queue.remove(0);
                    continue;
                }
                Ok(theirs) => {
                    return launch_merge(
                        cfg,
                        &call,
                        &theirs,
                        &ws,
                        &wc,
                        step_path,
                        &queue[1..],
                        &results,
                    )
                }
            }
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
                        &wc,
                        step_path,
                        &queue[1..],
                        &results,
                    )
                }
            }
        }
        // A built-in history tool (log/show/diff)? Like a tree tool, but the
        // script ships with the harness and it always gets the `#@git` context.
        if githist::is_builtin(name) && cfg.tools_image.is_some() {
            let tool = githist::tool(name).expect("is_builtin implies tool");
            match tools::tree_tool_args(&call, &tool) {
                Err(block) => {
                    results.push(block);
                    queue.remove(0);
                    continue;
                }
                Ok(bound) => {
                    return launch_githist(
                        cfg,
                        &call,
                        name,
                        &bound,
                        &ws,
                        &wc,
                        step_path,
                        &queue[1..],
                        &results,
                    )
                }
            }
        }
        // A tree tool? Resolved in the CURRENT workspace at invocation time,
        // so a call made right after an edit runs the edited script.
        if !tools::is_inline(name) && cfg.tools_image.is_some() {
            if let Some((script, tool)) = tools::tree_tool_script(&ws, name)? {
                // Bind the declared `#@arg`s before launching: a bad call is
                // an is_error result and the queue continues, exactly as a
                // bad grep is — only a valid one exits into the sub-run.
                match tools::tree_tool_args(&call, &tool) {
                    Err(block) => {
                        results.push(block);
                        queue.remove(0);
                        continue;
                    }
                    Ok(bound) => {
                        return launch_tree_tool(
                            cfg,
                            &call,
                            name,
                            &script,
                            &bound,
                            tool.git,
                            &ws,
                            &wc,
                            step_path,
                            &queue[1..],
                            &results,
                        )
                    }
                }
            }
        }
        if !tools::is_inline(name) {
            return Err(format!(
                "model called unknown tool {name:?} (built-ins: bash, grep, read, \
                 ls, write, edit, merge; plus this workspace's caos-tools/*.sh)"
            ));
        }
        let (block, new_ws) = tools::execute(&call, &ws)?;
        results.push(block);
        if let Some(w) = new_ws {
            // An inline MUTATION: advance the workspace and mint its child
            // commit (a read returns None and leaves both untouched).
            wc = advance_wc(&w, &wc, name)?;
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
    llm_round(cfg, messages, &ws, &wc, head_hash, &step_hash, &results)
}

/// Mint the child workspace commit after a mutation: `commit(new tree, parent
/// = current wc)`, recorded at a fresh commit path. The workspace-commit chain
/// this builds roots at the head commit, so a `merge`'s `M` (and thus its
/// `theirs`) is reachable once a step hangs the latest `wc` off itself.
fn advance_wc(ws: &str, wc: &str, what: &str) -> Result<String, String> {
    let tree = cas_hash(ws)?;
    let parent = cas_hash(wc)?;
    let out = fresh("wc");
    write_commit_as(&tree, &[&parent], what, agent_now(), &out)?;
    Ok(out)
}

/// One LLM API round over `messages`, with `ws` the workspace CAS path the
/// round is over, `wc` its commit, `prev` the commit the next step chains onto
/// (the previous step, or the human turn), and `sent_results` the tool_result
/// blocks this round's request carried (recorded in the step commit's
/// step.json).
#[allow(clippy::too_many_arguments)]
fn llm_round(
    cfg: &Config,
    messages: Vec<Value>,
    ws: &str,
    wc: &str,
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
        "tools": registry(cfg, ws)?,
        "messages": messages,
    });
    // Bracket the API call with status-ref updates (progress::status): the
    // call is the one silent, slow part of a turn, so say what it's doing —
    // and, via the retry callback, why it's waiting.
    let status = |text: &str| progress::status(cfg.conversation.as_deref(), head_hash, text);
    status(&format!("calling {}…", cfg.model));
    let started = std::time::Instant::now();
    let resp = post_messages(&cfg.base_url, &cfg.api_key, &body, &status)?;
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
                let (step_hash, _) = mint_step(cfg, ws, prev, wc, sent_results, &blocks)?;
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
            let (_, step_path) = mint_step(cfg, ws, prev, wc, sent_results, &blocks)?;
            drive(
                cfg,
                ws.to_string(),
                wc.to_string(),
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
/// FIRST parent = the previous step (or the human turn), SECOND parent = the
/// workspace commit `wc` (unless it already equals the first parent — the
/// turn's first round, before any tool ran). That second parent hangs the
/// workspace-commit chain — and so any `merge`'s `M` and its `theirs` — off the
/// transcript, reachable, without disturbing the first-parent spine or the
/// transcript walk. Author `caos-agent` at wall-clock time. Pushes the progress
/// ref (best-effort). Returns the commit's `(hash, cas-path)`.
fn mint_step(
    cfg: &Config,
    ws: &str,
    parent: &str,
    wc: &str,
    sent_results: &[Value],
    blocks: &[Value],
) -> Result<(String, String), String> {
    let dir = scratch("steptree")?;
    // Link every workspace entry EXCEPT `.caos` (rebuilt below): a mid-merge
    // workspace carries `.caos/conflicts`, which must survive alongside the
    // step.json we add — the two share the `.caos/` name, never a file.
    let mut ws_caos: Option<String> = None;
    for child in entries(ws)? {
        if file_name(&child) == STEP_DIR {
            ws_caos = Some(path(&child).to_string());
            continue;
        }
        link(&child, dir.join(file_name(&child)))?;
    }
    let caos_dir = dir.join(STEP_DIR);
    fs::create_dir(&caos_dir).map_err(|e| format!("creating {STEP_DIR}: {e}"))?;
    if let Some(ws_caos) = ws_caos {
        caos(["get", &ws_caos])?;
        for child in entries(&ws_caos)? {
            link(&child, caos_dir.join(file_name(&child)))?;
        }
    }
    let step_json = json!({
        "content": blocks,
        "results": sent_results,
        "v": 1,
    });
    fs::write(caos_dir.join(STEP_FILE), step_json.to_string())
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
    // The workspace commit as a second parent — omitted when it IS the first
    // parent (the turn's opening round, wc still the head commit): git allows a
    // duplicate parent but it is noise.
    let wc_hash = cas_hash(wc)?;
    let mut parents: Vec<&str> = vec![parent];
    if wc_hash != parent {
        parents.push(&wc_hash);
    }
    let commit_path = fresh("step");
    let hash = write_commit_as(&tree_hash, &parents, &message, agent_now(), &commit_path)?;
    if let Some(conversation) = &cfg.conversation {
        progress::push(conversation, &hash);
    }
    Ok((hash, commit_path))
}

/// Launch one bash call as a run-then sub-run: `in` = `{tree, cmd, paths}`,
/// `run` = the bash image, `then` = ourselves re-curried with the loop state.
fn launch(
    cfg: &Config,
    call: &Value,
    ws: &str,
    wc: &str,
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
        wc,
        step_path,
        pending,
        results,
        id,
        // `ws` rides even though a SUCCESSFUL bash callback takes the workspace
        // from `result/tree`: a CAUGHT failure has no result tree, and the queue
        // has to continue from the workspace as it stood before the call.
        &[("current-tool", Arg::Lit("bash")), ("ws", Arg::Path(ws))],
    )?;
    run_then_catching(&in_path, &cfg.bash_image, &me)
}

/// Launch a `merge` call as a run-then sub-run of the git-bearing merge worker
/// (std/merge): `ours` is the threaded workspace commit `wc`, `theirs` the
/// resolved commit, both curried onto the image as gitlink args; the worker
/// fetches their closures from the server, three-way-merges, and returns the
/// two-parent commit `M`. `--in` is immaterial (everything rides curried) but
/// run-then needs one. The result is a COMMIT, not a workspace tree — the
/// callback makes it the new workspace commit.
#[allow(clippy::too_many_arguments)]
fn launch_merge(
    cfg: &Config,
    call: &Value,
    theirs: &str,
    ws: &str,
    wc: &str,
    step_path: &str,
    pending: &[Value],
    results: &[Value],
) -> Result<(), String> {
    let id = call["id"]
        .as_str()
        .ok_or("tool_use block has no string id")?;
    let image = cfg
        .merge_image
        .as_ref()
        .ok_or("launch_merge without a merge_image (drive guards this)")?;
    // `theirs` is a bare hash (from the ref snapshot); materialize it as a
    // commit-kinded /cas path so it curries as a gitlink (`:commit=`), like wc.
    let theirs_path = fresh("theirs");
    caos(["get-hash", theirs, &theirs_path])?;
    let curried = caos_curry(
        image,
        &[("ours", Arg::Path(wc)), ("theirs", Arg::Path(&theirs_path))],
    )?;
    let me = self_curry(
        wc,
        step_path,
        pending,
        results,
        id,
        // As in `launch`: the success path rebuilds the workspace from the
        // merge commit, but a caught failure continues from this `ws`.
        &[("current-tool", Arg::Lit("merge")), ("ws", Arg::Path(ws))],
    )?;
    run_then_catching(ws, &curried, &me)
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
    wc: &str,
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
        wc,
        step_path,
        pending,
        results,
        id,
        &[
            ("current-tool", Arg::Lit("grep")),
            ("ws", Arg::Path(ws)),
            ("scope", Arg::Lit(scope_prefix)),
        ],
    )?;
    run_then_catching(scope, &curried, &me)
}

/// Launch a tree tool (`caos-tools/<name>.sh`, already resolved in the current
/// workspace) as a run-then sub-run: the input is the workspace tree and the
/// SCRIPT BLOB rides curried on the script-worker image, so the run caches
/// on exactly (workspace tree, script content, the bound `#@arg`s) — and an
/// edited tool is a new key automatically. The result is a value, not a
/// workspace — the current `ws` rides the continuation, unchanged by the run.
///
/// A `#@git` tool additionally gets the workspace commit (`wc`, a gitlink it
/// reads as the raw commit object — history's entry point via `caos get-hash`)
/// and the turn's ref snapshot (`refs`, the same `name <hash>` lines the merge
/// tool resolves `--theirs` against). Only `#@git` tools get them: `wc` moves
/// every step, so binding it into build/test would sink their caches.
#[allow(clippy::too_many_arguments)]
fn launch_tree_tool(
    cfg: &Config,
    call: &Value,
    name: &str,
    script: &str,
    bound: &[(String, String)],
    git: bool,
    ws: &str,
    wc: &str,
    step_path: &str,
    pending: &[Value],
    results: &[Value],
) -> Result<(), String> {
    let id = call["id"]
        .as_str()
        .ok_or("tool_use block has no string id")?;
    let image = cfg
        .tools_image
        .as_ref()
        .ok_or("launch_tree_tool without a tools_image (drive guards this)")?;
    // The model's arg values bind as LITERALS, so they land at
    // /cas/args/<name> for the script to read and — being part of the args
    // tree — key the run: the same tool called with a different hash is a
    // different job, not a cache hit.
    let mut kvs: Vec<(&str, Arg)> = vec![("worker1", Arg::Path(script))];
    kvs.extend(bound.iter().map(|(k, v)| (k.as_str(), Arg::Lit(v))));
    // History context, for `#@git` tools only. `wc` is commit-kinded, so it
    // curries as a gitlink (`:commit=`) and the tool reads it as the raw commit
    // object — exactly how the merge tool receives `ours`.
    if git {
        kvs.push(("wc", Arg::Path(wc)));
        if let Some(refs) = cfg.merge_refs.as_deref() {
            kvs.push(("refs", Arg::Lit(refs)));
        }
    }
    let curried = caos_curry(image, &kvs)?;
    let me = self_curry(
        wc,
        step_path,
        pending,
        results,
        id,
        &[("current-tool", Arg::Lit(name)), ("ws", Arg::Path(ws))],
    )?;
    run_then_catching(ws, &curried, &me)
}

/// Launch a built-in history tool (`log`/`show`/`diff`): assemble its embedded
/// script (`githist::script`), `caos put` it into CAS, and hand it to
/// [`launch_tree_tool`] with `git = true`. So it runs on the tree-tool image
/// with the `#@git` context and its result is rendered by the same callback
/// arm — the only difference from a project tool is where the script comes from.
#[allow(clippy::too_many_arguments)]
fn launch_githist(
    cfg: &Config,
    call: &Value,
    name: &str,
    bound: &[(String, String)],
    ws: &str,
    wc: &str,
    step_path: &str,
    pending: &[Value],
    results: &[Value],
) -> Result<(), String> {
    let body = githist::script(name).ok_or_else(|| format!("no built-in script for {name}"))?;
    let dir = scratch(&format!("githist-{name}"))?;
    let file = dir.join("worker.sh");
    fs::write(&file, body).map_err(|e| format!("writing {name} script: {e}"))?;
    let script = fresh("githist-script");
    caos(["put", path(&file), &script])?;
    launch_tree_tool(
        cfg, call, name, &script, bound, true, ws, wc, step_path, pending, results,
    )
}

/// Rebuild ourselves as the `then` for the next round — the same ArgTree we're
/// running as, with the loop state advanced. We carry our WHOLE current ArgTree
/// forward with [`own_args_tree`] (so the static config — `api-key`, `system`,
/// `bash-image`, `head`, `worker1`, and the optional
/// `model`/`base-url`/`conversation`/`grep-image`/`tools-image`/`merge-image`/`merge-refs`
/// — rides along and a NEW config arg needs no edit here) and manage only the
/// args this loop OWNS: unbind whichever are bound right now, then rebind the
/// ones that continue. Commit-valued paths (`head`, `step`, `wc`) ride as
/// gitlinks. Contrast the old approach — rebuild from the bare base and re-list
/// every arg — whose keep-list dropped a config arg the moment you forgot to add
/// it; here a forgotten carry is impossible and a forgotten unbind is a loud
/// rebind error, not a stale value.
fn self_curry(
    wc: &str,
    step_path: &str,
    pending: &[Value],
    results: &[Value],
    current_id: &str,
    extras: &[(&str, Arg)],
) -> Result<String, String> {
    let pending_json = Value::Array(pending.to_vec()).to_string();
    let results_json = Value::Array(results.to_vec()).to_string();

    // The loop/per-call args this function owns. Unbind whichever are bound in
    // the current invocation (so they neither double-bind nor persist), then
    // rebind the state below and the per-tool `extras`; `in`/`result` (run-then's
    // call args) are dropped — the server supplies fresh ones next call.
    //   loop state (rebound below): step, wc, pending, results, current-id
    //   per-tool (rebound via `extras`): current-tool, ws, scope
    //   run-then's call args (dropped): in, result
    const MANAGED: &[&str] = &[
        "step",
        "wc",
        "pending",
        "results",
        "current-id",
        "current-tool",
        "ws",
        "scope",
        "in",
        "result",
        // run-then's other call arg: bound instead of `result` when `--catch`
        // delivered a failure. Unbound here like `result`, or the next call
        // would inherit a stale error and re-report it.
        "error",
    ];
    let unbind: Vec<&str> = MANAGED
        .iter()
        .copied()
        .filter(|name| Path::new(&arg(name)).exists())
        .collect();

    let mut kvs: Vec<(&str, Arg)> = vec![
        ("step", Arg::Path(step_path)),
        // The workspace commit the callback continues from (a gitlink).
        ("wc", Arg::Path(wc)),
        ("pending", Arg::Lit(&pending_json)),
        ("results", Arg::Lit(&results_json)),
        ("current-id", Arg::Lit(current_id)),
    ];
    for (name, value) in extras {
        kvs.push((
            name,
            match value {
                Arg::Lit(s) => Arg::Lit(s),
                Arg::Path(s) => Arg::Path(s),
            },
        ));
    }
    caos_recurry(&own_args_tree()?, &unbind, &kvs)
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

/// The full tool registry: bash, grep and the cargo tools (the sub-run tools)
/// plus the inline file tools (`tools.rs`).
fn registry(cfg: &Config, ws: &str) -> Result<Vec<Value>, String> {
    let mut tools = vec![bash_tool()];
    tools.extend(tools::declarations());
    if cfg.grep_image.is_some() {
        tools.push(tools::grep_declaration());
    }
    if cfg.merge_image.is_some() {
        tools.push(merge_tool());
    }
    // Tree tools: whatever caos-tools/*.sh the CURRENT workspace carries —
    // re-discovered every round, so the set tracks the agent's own edits.
    if cfg.tools_image.is_some() {
        // The built-in history tools (log/show/diff) run on the same std/bash
        // image as tree tools, so they share its gate.
        tools.extend(githist::declarations());
        for tool in tools::tree_tools(ws)? {
            tools.push(tools::tree_tool_declaration(&tool));
        }
    }
    Ok(tools)
}

/// The `merge` tool's registry entry.
fn merge_tool() -> Value {
    json!({
        "name": "merge",
        "description": "Three-way merge another commit into the current workspace. `theirs` is \
    a ref name from the snapshot (e.g. `main`, `origin/main`) or a commit hash; the current \
    side is the workspace as it is now. A clean merge advances the workspace to the merged \
    result. A conflict advances it too, with git's inline conflict markers in the files and a \
    reserved `.caos/conflicts` file listing every unresolved path — including structural \
    conflicts (delete/modify, mode, binary) that have NO markers. Resolve each: edit the file \
    (use `read` with the stage's oid as `root` to inspect its content), then delete that path's rows \
    from `.caos/conflicts`. Then build and test.",
        "input_schema": {
            "type": "object",
            "properties": {
                "theirs": {"type": "string", "description": "The commit to merge in: a ref name from the snapshot, or a commit hash."}
            },
            "required": ["theirs"]
        }
    })
}

/// Resolve a `merge` call's `--theirs` against the turn-start ref snapshot
/// (SPEC "Resolving `--theirs`"): a known ref name → its hash, a bare hash →
/// itself, else an is_error tool_result listing the available names.
fn resolve_theirs(cfg: &Config, call: &Value) -> Result<String, Value> {
    let id = call["id"].as_str().unwrap_or("");
    let theirs = call["input"]["theirs"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    lookup_theirs(cfg.merge_refs.as_deref(), theirs).map_err(|msg| {
        json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": [{"type": "text", "text": msg}],
            "is_error": true,
        })
    })
}

/// The pure core of [`resolve_theirs`]: given the snapshot blob (`name <hash>`
/// lines) and the requested `theirs`, return the hash or a user-facing error.
fn lookup_theirs(refs: Option<&str>, theirs: Option<&str>) -> Result<String, String> {
    let theirs = theirs
        .ok_or_else(|| "merge needs a string `theirs` (a ref name or a commit hash)".to_string())?;
    let mut names = Vec::new();
    for line in refs.unwrap_or("").lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((name, hash)) = line.split_once(char::is_whitespace) {
            let (name, hash) = (name.trim(), hash.trim());
            if name == theirs {
                return Ok(hash.to_string());
            }
            names.push(name.to_string());
        }
    }
    // A bare hash is always allowed (sha1/sha256), so the model can merge a
    // commit it learned by hash even if it isn't a named ref.
    if (theirs.len() == 40 || theirs.len() == 64) && theirs.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(theirs.to_string());
    }
    Err(format!(
        "unknown merge target {theirs:?}; available refs: {}",
        if names.is_empty() {
            "(none)".to_string()
        } else {
            names.join(", ")
        }
    ))
}

/// The tool_result for a finished merge: clean, or the conflicted paths the
/// model must resolve (from `M`'s `.caos/conflicts`). `ws` is `M`'s tree,
/// already materialized one level.
fn merge_result_block(id: &str, ws: &str) -> Result<Value, String> {
    let caos_dir = format!("{ws}/{STEP_DIR}");
    let mut conflicts = None;
    if Path::new(&caos_dir).exists() {
        caos(["get", &caos_dir])?;
        let file = format!("{caos_dir}/conflicts");
        if Path::new(&file).exists() {
            caos(["get", &file])?;
            conflicts =
                Some(fs::read_to_string(&file).map_err(|e| format!("reading conflicts: {e}"))?);
        }
    }
    let text = match conflicts {
        Some(body) => format!(
            "merge produced conflicts. The workspace now carries git's inline conflict markers \
             in the affected files, plus .caos/conflicts (git's unmerged notation, richer than \
             markers). Resolve each path — edit the file, reading a stage's content with `read` \
             (pass the stage oid as `root`) — then delete that path's rows from .caos/conflicts. Build and \
             test when done.\n\n.caos/conflicts:\n{}",
            body.trim_end()
        ),
        None => "merge completed cleanly; the workspace is the merged result.".to_string(),
    };
    Ok(json!({
        "type": "tool_result",
        "tool_use_id": id,
        "content": [{"type": "text", "text": text}],
    }))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theirs_lookup() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let refs = format!("main {a}\norigin/main {b}\n");
        // A known ref name resolves to its snapshotted hash.
        assert_eq!(lookup_theirs(Some(&refs), Some("main")).unwrap(), a);
        assert_eq!(lookup_theirs(Some(&refs), Some("origin/main")).unwrap(), b);
        // A bare hash passes through even when it is not a named ref.
        let c = "c".repeat(40);
        assert_eq!(lookup_theirs(Some(&refs), Some(&c)).unwrap(), c);
        // An unknown name errors, listing the available names.
        let e = lookup_theirs(Some(&refs), Some("nope")).unwrap_err();
        assert!(e.contains("main") && e.contains("origin/main"), "{e}");
        // A missing/blank target errors.
        assert!(lookup_theirs(Some(&refs), None).is_err());
        // No snapshot at all: a bare hash still works, a name reports "(none)".
        assert_eq!(lookup_theirs(None, Some(&a)).unwrap(), a);
        assert!(lookup_theirs(None, Some("main"))
            .unwrap_err()
            .contains("(none)"));
    }
}
