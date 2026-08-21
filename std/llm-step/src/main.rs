//! caos-worker-llm-step: the agent-harness driver (see design/agent-harness.md).
//!
//! The canonical conversation event spine is the loop state. A start or a
//! run-then callback rereads it, performs the next missing action, and records
//! that action's result before continuing.
//!
//! Tool calls are driven serially through one queue (`drive`): the inline file
//! tools (read/ls/write/edit — `tools.rs`) execute in-process, advancing the
//! workspace with no sub-run; compute tools exit into serial run-then sub-runs.
//! Continuations carry only the stable run/round/call identity and observed
//! head; pending queues and results are always reconstructed from the ref.

mod async_work;
mod githist;
mod progress;
mod subagents;
mod tools;

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use llm_client::{post_messages, DEFAULT_BASE_URL};
use serde_json::{json, Value};
use worker_common::{
    arg, caos, caos_curry, caos_recurry, cas_hash, eval_then_catching, forward, link,
    own_args_tree, path, read_arg, read_arg_opt, read_commit, run_then_catching, run_worker,
    scratch, secret, write_commit_as, Arg,
};

const AGENT_AUTHOR: &str = "caos-agent";
const STEP_DIR: &str = ".caos";

/// The per-round output-token cap sent to the API. A single response is
/// unlikely to need this much; when one does, `stop_reason: "max_tokens"`
/// truncates it and we continue (see `llm_round`).
const MAX_TOKENS: u64 = 64000;

/// How many times a single round may be continued after a `max_tokens`
/// truncation before the turn gives up. Each continuation prefills the partial
/// response and asks the model to resume, so the bound caps a pathological
/// loop (a model that never stops) at `MAX_CONTINUATIONS + 1` API calls.
const MAX_CONTINUATIONS: u32 = 8;

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
    /// The script-worker image (std/bash) the BUILT-IN HISTORY TOOLS run on
    /// (log/show/diff). A tree tool names its own image in its `.caos-expr`
    /// (SPEC, "Tools"), so it needs nothing from here. Registered when present.
    tools_image: Option<String>,
    /// The git-bearing merge worker (std/merge). The `merge` tool is registered
    /// only when present.
    merge_image: Option<String>,
    run_and_update_ref_image: Option<String>,
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
        // Keep run-and-update-ref-image in a child request so the request stays
        // a superset of the configured llm-step secret reader. The marker, not
        // removal of that identity-bearing argument, disables nested agents.
        let run_and_update_ref_image = if read_arg_opt("subagent")?.is_some() {
            None
        } else {
            image_arg("run-and-update-ref-image")?
        };
        Ok(Config {
            api_key: secret("anthropic-api-key")?,
            system: read_arg("system")?,
            bash_image: image_arg("bash-image")?.ok_or("--bash-image is required")?,
            grep_image: image_arg("grep-image")?,
            tools_image: image_arg("tools-image")?,
            merge_image: image_arg("merge-image")?,
            run_and_update_ref_image,
            merge_refs: read_arg_opt("merge-refs")?,
            model: read_arg("model")?,
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
    let outcome = if Path::new(&arg("result")).exists() || Path::new(&arg("error")).exists() {
        callback(&cfg)
    } else {
        start(&cfg)
    };
    if let Err(error) = &outcome {
        if let Err(record_error) = record_failure(&cfg, error) {
            eprintln!("llm-step: additionally failed to record failure: {record_error}");
        }
    }
    outcome
}

fn start(cfg: &Config) -> Result<(), String> {
    let conversation = conversation(cfg)?;
    let run = own_args_tree()?;
    let head_hash = cas_hash(&arg("head"))?;
    let log = ensure_request_running(conversation, &run, &head_hash)?;
    if let Some(terminal) = terminal_for_run(&log, &run)? {
        return finish_from_terminal(cfg, &log, terminal);
    }
    // One recovery dispatch at turn entry is enough. Normal tool callbacks do
    // not repeat it and park another single-flight waiter for every tool call.
    reconcile_async_tasks(cfg, &log)?;
    let request = active_request(&log)?;
    if request != run {
        return Err(format!(
            "conversation's active request is {request}, but this worker is {run}"
        ));
    }
    let (ws, _) = canonical_workspace(&log)?;
    if log.events.len() <= 2 && Path::new(&ws).join(STEP_DIR).exists() {
        return Err(format!(
            "the conversation's base tree already contains the reserved {STEP_DIR:?} entry"
        ));
    }
    resume_run(cfg, &run, &head_hash, log)
}

/// Claim an exact request already visible on the canonical event spine before
/// doing any model work. `head` is the user event used to construct the request;
/// its admission child records both hashes atomically. The worker owns the
/// transition to running. A user event may win the ref race while we do this,
/// so each retry revalidates the same anchor and appends after the new tip.
fn ensure_request_running(
    conversation: &str,
    run: &str,
    head: &str,
) -> Result<progress::ConversationLog, String> {
    validate_run_hash(run)?;
    validate_run_hash(head)?;
    for _ in 0..32 {
        let log = progress::conversation_log(conversation)?;
        match request_start_disposition(&log, run, head)? {
            RequestStart::Running => return Ok(log),
            RequestStart::Claim { expected, tree } => {
                let event = json!({"request": run, "status": "running"});
                match progress::append_event_at_head(conversation, &expected, &event, &tree)? {
                    progress::ConditionalAppend::Appended(_) => {
                        return progress::conversation_log(conversation)
                    }
                    progress::ConditionalAppend::HeadChanged(_) => continue,
                }
            }
        }
    }
    Err("conversation kept changing while starting request".to_string())
}

/// Callback from run-then: `result` is the sub-run tool's result, `in` the
/// call it answered (unused — `current_id` carries the id), and the rest of
/// the loop state rode our own curry. Establishes the workspace (`ws`) and the
/// workspace commit (`wc`) the queue continues over.
fn callback(cfg: &Config) -> Result<(), String> {
    let conversation = conversation(cfg)?;
    let log = progress::conversation_log(conversation)?;
    let head_hash = cas_hash(&arg("head"))?;
    let run = read_arg("run")?;
    let round = read_arg("round")?
        .parse::<u64>()
        .map_err(|error| format!("invalid continuation round: {error}"))?;
    let base_head = read_arg("base-head")?;
    if let Some(terminal) = terminal_for_run(&log, &run)? {
        return finish_from_terminal(cfg, &log, terminal);
    }
    let request = active_request(&log)?;
    let expected_request = request_for_head(&log, &head_hash)?;
    if request != expected_request || request != run {
        return Err(format!(
            "callback belongs to request {expected_request} ({run}), but conversation is running {request}"
        ));
    }
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
        let result = json!({
            "type": "tool_result",
            "tool_use_id": current_id,
            "is_error": true,
            "content": [{"type": "text", "text": format!(
                "the `{current_tool}` tool failed to run: {}\n\nThe workspace is unchanged. \
                 This is the tool itself failing, not a non-zero exit from your command.",
                text.trim_end()
            )}],
        });
        append_tool_result(
            cfg,
            &run,
            round,
            &base_head,
            &result,
            &cas_hash(&arg("ws"))?,
            None,
        )?;
        return resume_run(
            cfg,
            &run,
            &head_hash,
            progress::conversation_log(conversation)?,
        );
    }

    // STAGE TWO of a tree-tool call: this `--result` is not a tool's answer but
    // the tool's ARG TREE, from evaluating its `.caos-expr` (see
    // `launch_tree_tool`). Curry the model's args onto it and run it; the tool's
    // real result comes back to the arm below on the next hop. The failure case
    // is already handled above — a broken expression reaches the model as an
    // is_error tool_result like any other tool failure.
    if let Some(tool) = read_arg_opt("tool-eval")? {
        return launch_evaluated_tool(
            cfg,
            &tool,
            &arg("ws"),
            &arg("wc"),
            &run,
            round,
            &base_head,
            &current_id,
        );
    }

    match current_tool.as_str() {
        "grep" => {
            let scope = read_arg_opt("scope")?.unwrap_or_default();
            let result = tools::grep_result_block(&current_id, &arg("result"), &scope)?;
            append_tool_result(
                cfg,
                &run,
                round,
                &base_head,
                &result,
                &cas_hash(&arg("ws"))?,
                None,
            )?;
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
            let result = merge_result_block(&current_id, &ws)?;
            append_tool_result(
                cfg,
                &run,
                round,
                &base_head,
                &result,
                &commit.tree,
                Some(&cas_hash(&m)?),
            )?;
        }
        // A tree tool's result (caos-tools/<name>/) is a VALUE — a report,
        // a bin tree, diagnostics — never a workspace: the pre-run workspace
        // and its commit rode our curry, exactly like grep.
        name if name != "bash" => {
            let result = tools::tree_tool_result_block(&current_id, &arg("result"))?;
            append_tool_result(
                cfg,
                &run,
                round,
                &base_head,
                &result,
                &cas_hash(&arg("ws"))?,
                None,
            )?;
        }
        _ => {
            let result = tool_result_block(&current_id)?;
            let ws = format!("{}/tree", arg("result"));
            if !Path::new(&ws).exists() {
                return Err("bash result carries no `tree` entry".to_string());
            }
            caos(["get", &ws])?;
            // bash may have mutated the tree — advance the workspace commit.
            let wc = advance_wc(&ws, &arg("wc"), "bash")?;
            append_tool_result(
                cfg,
                &run,
                round,
                &base_head,
                &result,
                &cas_hash(&ws)?,
                Some(&cas_hash(&wc)?),
            )?;
        }
    }

    resume_run(
        cfg,
        &run,
        &head_hash,
        progress::conversation_log(conversation)?,
    )
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
    queue: &[Value],
    run: &str,
    round: u64,
    mut base_head: String,
) -> Result<(), String> {
    let mut queue = queue.to_vec();
    while let Some(call) = queue.first().cloned() {
        let observed = progress::conversation_log(conversation(cfg)?)?;
        if escaped_for_run(&observed, run)? {
            return finish_interrupted(cfg, run, observed);
        }
        let name = call["name"].as_str().unwrap_or("");
        if name == subagents::SPAWN_TOOL {
            let image = cfg
                .run_and_update_ref_image
                .as_deref()
                .ok_or("spawn_agent was called without a run-and-update-ref image")?;
            let log = progress::conversation_log(conversation(cfg)?)?;
            let owner = request_owner(&log, head_hash)?.to_string();
            let result = subagents::spawn_call(
                &call,
                conversation(cfg)?,
                &owner,
                run,
                round,
                &ws,
                &wc,
                &cfg.system,
                image,
                |task| ensure_async_status(cfg, task),
            )?;
            let log = progress::conversation_log(conversation(cfg)?)?;
            (ws, _) = canonical_workspace(&log)?;
            base_head = log.head;
            append_tool_result(cfg, run, round, &base_head, &result, &cas_hash(&ws)?, None)?;
            let log = progress::conversation_log(conversation(cfg)?)?;
            (ws, wc) = canonical_workspace(&log)?;
            base_head = log.head;
            queue.remove(0);
            continue;
        }
        if name == async_work::TOOL_NAME {
            let image = cfg
                .run_and_update_ref_image
                .as_deref()
                .ok_or("run_async was called without a run-and-update-ref image")?;
            let result = async_work::queue_call(&call, conversation(cfg)?, image, |task| {
                ensure_async_status(cfg, task)
            })?;

            // Pending publication or the background task may have advanced F.
            // Fold this tree-neutral tool result over the actual canonical tip,
            // never over the stale workspace used to construct pending.
            let log = progress::conversation_log(conversation(cfg)?)?;
            (ws, _) = canonical_workspace(&log)?;
            base_head = log.head;
            append_tool_result(cfg, run, round, &base_head, &result, &cas_hash(&ws)?, None)?;
            let log = progress::conversation_log(conversation(cfg)?)?;
            (ws, wc) = canonical_workspace(&log)?;
            base_head = log.head;
            queue.remove(0);
            continue;
        }
        if name == "bash" {
            return launch(cfg, &call, &ws, &wc, run, round, &base_head);
        }
        if name == "merge" && cfg.merge_image.is_some() {
            match resolve_theirs(cfg, &call) {
                Err(block) => {
                    append_tool_result(cfg, run, round, &base_head, &block, &cas_hash(&ws)?, None)?;
                    let log = progress::conversation_log(conversation(cfg)?)?;
                    (ws, wc) = canonical_workspace(&log)?;
                    base_head = log.head;
                    queue.remove(0);
                    continue;
                }
                Ok(theirs) => {
                    return launch_merge(cfg, &call, &theirs, &ws, &wc, run, round, &base_head)
                }
            }
        }
        if name == "grep" && cfg.grep_image.is_some() {
            // Validate before launching: a bad pattern or scope is an
            // is_error result and the queue continues — only a valid call
            // exits into the fold sub-run.
            match tools::grep_precheck(&call, &ws) {
                Err(block) => {
                    append_tool_result(cfg, run, round, &base_head, &block, &cas_hash(&ws)?, None)?;
                    let log = progress::conversation_log(conversation(cfg)?)?;
                    (ws, wc) = canonical_workspace(&log)?;
                    base_head = log.head;
                    queue.remove(0);
                    continue;
                }
                Ok((scope, prefix)) => {
                    return launch_grep(
                        cfg, &call, &scope, &prefix, &ws, &wc, run, round, &base_head,
                    )
                }
            }
        }
        // A built-in history tool (log/show/diff)? Like a tree tool, but the
        // script ships with the harness and it always gets the `@git` context.
        if githist::is_builtin(name) && cfg.tools_image.is_some() {
            let tool = githist::tool(name).expect("is_builtin implies tool");
            match tools::tree_tool_args(&call, &tool) {
                Err(block) => {
                    append_tool_result(cfg, run, round, &base_head, &block, &cas_hash(&ws)?, None)?;
                    let log = progress::conversation_log(conversation(cfg)?)?;
                    (ws, wc) = canonical_workspace(&log)?;
                    base_head = log.head;
                    queue.remove(0);
                    continue;
                }
                Ok(bound) => {
                    return launch_githist(
                        cfg, &call, name, &bound, &ws, &wc, run, round, &base_head,
                    )
                }
            }
        }
        // A tree tool? Resolved in the CURRENT workspace at invocation time,
        // so a call made right after an edit runs the edited tool.
        if !tools::is_inline(name) {
            if let Some(tool) = tools::tree_tool(&ws, name)? {
                // Bind the declared `@param`s before launching: a bad call is
                // an is_error result and the queue continues, exactly as a
                // bad grep is — only a valid one exits into the sub-run.
                match tools::tree_tool_args(&call, &tool) {
                    Err(block) => {
                        append_tool_result(
                            cfg,
                            run,
                            round,
                            &base_head,
                            &block,
                            &cas_hash(&ws)?,
                            None,
                        )?;
                        let log = progress::conversation_log(conversation(cfg)?)?;
                        (ws, wc) = canonical_workspace(&log)?;
                        base_head = log.head;
                        queue.remove(0);
                        continue;
                    }
                    Ok(bound) => {
                        return launch_tree_tool(
                            &call, name, &bound, tool.git, &ws, &wc, run, round, &base_head,
                        )
                    }
                }
            }
        }
        if !tools::is_inline(name) {
            return Err(format!(
                "model called unknown tool {name:?} (built-ins: bash, grep, read, \
                 ls, write, edit, merge, spawn_agent; plus this \
                 workspace's caos-tools/<name>/ tools)"
            ));
        }
        let (block, new_ws) = tools::execute(&call, &ws)?;
        let mut extra_parent = None;
        if let Some(w) = new_ws {
            // An inline MUTATION: advance the workspace and mint its child
            // commit (a read returns None and leaves both untouched).
            wc = advance_wc(&w, &wc, name)?;
            ws = w;
            extra_parent = Some(cas_hash(&wc)?);
        }
        append_tool_result(
            cfg,
            run,
            round,
            &base_head,
            &block,
            &cas_hash(&ws)?,
            extra_parent.as_deref(),
        )?;
        let log = progress::conversation_log(conversation(cfg)?)?;
        (ws, wc) = canonical_workspace(&log)?;
        base_head = log.head;
        queue.remove(0);
    }

    let log = progress::conversation_log(conversation(cfg)?)?;
    resume_run(cfg, run, head_hash, log)
}

/// Mint the child workspace commit after a mutation: `commit(new tree, parent
/// = current wc)`, recorded at a fresh commit path. The workspace-commit chain
/// this builds roots at the head commit, so a `merge`'s `M` (and thus its
/// `theirs`) is reachable once a step hangs the latest `wc` off itself.
fn advance_wc(ws: &str, wc: &str, what: &str) -> Result<String, String> {
    let tree = cas_hash(ws)?;
    let timestamp = read_commit_timestamp(wc)?;
    let parent = cas_hash(wc)?;
    let out = fresh("wc");
    // A retry of the same durable call must mint the same ancestry object. The
    // parent's date is stable and keeps published workspace commits alongside
    // the user turn that caused them.
    write_commit_as(
        &tree,
        &[&parent],
        what,
        Some((AGENT_AUTHOR, timestamp)),
        &out,
    )?;
    Ok(out)
}

/// Read only the committer timestamp needed for a causal workspace commit.
/// Keep this llm-step-specific concern out of worker-common: changing that
/// bootstrap-bound source changes the seeded rustc result.
fn read_commit_timestamp(cas_path: &str) -> Result<i64, String> {
    caos(["get", cas_path])?;
    let text = fs::read_to_string(cas_path).map_err(|e| format!("reading {cas_path}: {e}"))?;
    parse_commit_timestamp(&text)
}

fn parse_commit_timestamp(text: &str) -> Result<i64, String> {
    let (headers, _) = text
        .split_once("\n\n")
        .ok_or_else(|| format!("malformed commit (no blank line): {text:?}"))?;
    headers
        .lines()
        .find(|line| line.starts_with("committer "))
        .and_then(|line| line.split_whitespace().rev().nth(1))
        .ok_or_else(|| format!("commit has no committer timestamp: {text:?}"))?
        .parse::<i64>()
        .map_err(|error| format!("commit has an invalid committer timestamp: {error}"))
}

/// One LLM API round over `messages`. `prev` is the exact canonical head used
/// to build the request; publication is conditional on that head so a response
/// can never claim to have seen a concurrent interjection.
#[allow(clippy::too_many_arguments)]
fn llm_round(
    cfg: &Config,
    messages: Vec<Value>,
    ws: &str,
    head_hash: &str,
    prev: &str,
    run: &str,
    round: u64,
) -> Result<(), String> {
    let body = json!({
        "model": cfg.model,
        "max_tokens": MAX_TOKENS,
        // Constrains model choice: adaptive thinking needs a 4.6+ model
        // (haiku-4-5 rejects it with a 400). Deliberately unconditional —
        // sniffing per-model capabilities here would rot.
        "thinking": {"type": "adaptive"},
        "cache_control": {"type": "ephemeral"},
        "system": cfg.system,
        "tools": registry(cfg, ws)?,
        "messages": messages,
    });
    let status = |text: &str| eprintln!("llm-step: {text}");
    // A single logical round can span several API calls. When a response ends
    // with stop_reason "max_tokens" it was truncated mid-generation; rather
    // than fail the turn, we append the partial assistant content as a prefill
    // and ask the model to resume (the API concatenates trailing assistant
    // messages), accumulating every round's blocks into one. Only end_turn and
    // tool_use end the loop; exhausting the continuation budget falls through
    // to the stop-reason match below, which still fails the turn.
    let mut messages = messages;
    let mut blocks: Vec<Value> = Vec::new();
    let mut stop;
    let mut continuation = 0u32;
    loop {
        if continuation == 0 {
            status(&format!("calling {}…", cfg.model));
        } else {
            status(&format!(
                "{} hit the {MAX_TOKENS}-token cap; continuing ({continuation}/{MAX_CONTINUATIONS})…",
                cfg.model
            ));
        }
        let mut body = body.clone();
        body["messages"] = Value::Array(messages.clone());
        let started = std::time::Instant::now();
        let resp = post_messages(&cfg.base_url, &cfg.api_key, &body, &status)?;
        status(&format!(
            "{} answered in {:.1}s",
            cfg.model,
            started.elapsed().as_secs_f64()
        ));
        stop = resp["stop_reason"].as_str().unwrap_or("").to_string();
        let round_blocks = resp["content"]
            .as_array()
            .cloned()
            .ok_or("API response has no content array")?;
        blocks.extend(round_blocks.iter().cloned());
        if stop == "max_tokens" && continuation < MAX_CONTINUATIONS {
            // Prefill the next call with the partial content so the model
            // picks up where it stopped.
            messages.push(message("assistant", Value::Array(round_blocks)));
            continuation += 1;
            continue;
        }
        break;
    }
    let tool_uses: Vec<Value> = blocks
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .cloned()
        .collect();
    // Prove the response shape is replayable before either terminal arm can
    // publish it. In particular, an `end_turn` carrying tool calls would leave
    // a permanently incomplete batch on the append-only conversation spine.
    let durable_calls = validated_tool_calls(&stop, &tool_uses)?;

    match stop.as_str() {
        "end_turn" => {
            let text = response_text(&blocks);
            let tree = cas_hash(ws)?;
            let conversation = conversation(cfg)?;
            let event = json!({
                "request": run,
                "round": round,
                "author": "assistant",
                "model": &cfg.model,
                "content": text,
                "response": &blocks,
                "status": "idle",
            });
            match progress::append_event_at_head(conversation, prev, &event, &tree)? {
                progress::ConditionalAppend::Appended(appended) => {
                    // A single post-turn recovery point closes dispatch loss
                    // before this llm-step process exits. Later recovery is
                    // owned by a fresh turn/follower, not by tool callbacks.
                    reconcile_async_tasks(cfg, &progress::conversation_log(conversation)?)?;
                    forward_commit(&appended.commit)
                }
                progress::ConditionalAppend::HeadChanged(_) => {
                    let log = progress::conversation_log(conversation)?;
                    if escaped_after(&log, prev, run)? {
                        record_interrupted_response(cfg, run, round, &blocks, None, log)
                    } else {
                        resume_run(cfg, run, head_hash, log)
                    }
                }
            }
        }
        "tool_use" => {
            let calls = durable_calls.ok_or("validated tool_use response has no calls")?;
            let tree = cas_hash(ws)?;
            let event = json!({
                "request": run,
                "round": round,
                "author": "assistant",
                "model": &cfg.model,
                "content": response_text(&blocks),
                "response": &blocks,
                "calls": &calls,
            });
            match progress::append_event_at_head(conversation(cfg)?, prev, &event, &tree)? {
                progress::ConditionalAppend::Appended(_) => resume_run(
                    cfg,
                    run,
                    head_hash,
                    progress::conversation_log(conversation(cfg)?)?,
                ),
                progress::ConditionalAppend::HeadChanged(_) => {
                    let log = progress::conversation_log(conversation(cfg)?)?;
                    if escaped_after(&log, prev, run)? {
                        record_interrupted_response(
                            cfg,
                            run,
                            round,
                            &blocks,
                            Some(&calls),
                            log,
                        )
                    } else {
                        resume_run(cfg, run, head_hash, log)
                    }
                }
            }
        }
        "max_tokens" => Err(format!(
            "LLM round still hit stop_reason \"max_tokens\" after {MAX_CONTINUATIONS} \
             continuation(s); the response would not converge and the turn fails here"
        )),
        other => Err(format!(
            "LLM round ended with stop_reason {other:?} (only end_turn and tool_use \
             are handled; the turn fails here by design for now)"
        )),
    }
}

/// Launch one bash call as a run-then sub-run: `in` = `{tree, cmd, paths}`,
/// `run` = the bash image, `then` = ourselves re-curried with the loop state.
fn launch(
    cfg: &Config,
    call: &Value,
    ws: &str,
    wc: &str,
    run: &str,
    round: u64,
    base_head: &str,
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
        run,
        round,
        base_head,
        id,
        // `ws` rides even though a SUCCESSFUL bash callback takes the workspace
        // from `result/tree`: a CAUGHT failure has no result tree, and the queue
        // has to continue from the workspace as it stood before the call.
        &[("current-tool", Arg::Lit("bash")), ("ws", Arg::Path(ws))],
    )?;
    run_then_catching(&in_path, Arg::Hash(&cfg.bash_image), Arg::Hash(&me))
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
    run: &str,
    round: u64,
    base_head: &str,
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
        Arg::Hash(image),
        &[("ours", Arg::Path(wc)), ("theirs", Arg::Path(&theirs_path))],
    )?;
    let me = self_curry(
        wc,
        run,
        round,
        base_head,
        id,
        // As in `launch`: the success path rebuilds the workspace from the
        // merge commit, but a caught failure continues from this `ws`.
        &[("current-tool", Arg::Lit("merge")), ("ws", Arg::Path(ws))],
    )?;
    run_then_catching(ws, Arg::Hash(&curried), Arg::Hash(&me))
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
    run: &str,
    round: u64,
    base_head: &str,
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
    let curried = caos_curry(Arg::Hash(image), &[("pattern", Arg::Lit(pattern))])?;
    let me = self_curry(
        wc,
        run,
        round,
        base_head,
        id,
        &[
            ("current-tool", Arg::Lit("grep")),
            ("ws", Arg::Path(ws)),
            ("scope", Arg::Lit(scope_prefix)),
        ],
    )?;
    run_then_catching(scope, Arg::Hash(&curried), Arg::Hash(&me))
}

/// Launch a tree tool (`caos-tools/<name>/`, already resolved in the current
/// workspace) — STAGE ONE of two, because a tool is now a directory carrying a
/// `.caos-expr` and reaching it means EVALUATING that expression.
///
/// Evaluating dispatches the runs the expression names (a compiled tool builds)
/// and blocks on them, which a worker may not do. So this stage's whole body is
/// the tail call that asks the server: `eval-path-then` over the workspace with
/// `caos-tools/<name>` as the path, and ourselves as the `then`
/// (design/caos-expr.md, "Who runs the walk"). [`launch_evaluated_tool`] is the
/// other side, where `--result` is the tool's ArgTree.
///
/// `--catch`: an expression that fails to evaluate — a tool whose build is
/// broken, most likely, since evaluating it is what builds it — comes back to
/// the model as an `is_error` tool_result like any other tool failure, rather
/// than taking the turn down.
///
/// The model's args ride across as `tool-args` (a JSON object of the bound
/// `@param`s) because the curry cannot happen until the ArgTree exists.
#[allow(clippy::too_many_arguments)]
fn launch_tree_tool(
    call: &Value,
    name: &str,
    bound: &[(String, String)],
    git: bool,
    ws: &str,
    wc: &str,
    run: &str,
    round: u64,
    base_head: &str,
) -> Result<(), String> {
    let id = call["id"]
        .as_str()
        .ok_or("tool_use block has no string id")?;
    let args: serde_json::Map<String, Value> = bound
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    let args = Value::Object(args).to_string();
    let me = self_curry(
        wc,
        run,
        round,
        base_head,
        id,
        &[
            ("current-tool", Arg::Lit(name)),
            ("ws", Arg::Path(ws)),
            // The marker that tells the callback this `--result` is an ArgTree
            // to run, not a tool's answer to render.
            ("tool-eval", Arg::Lit(name)),
            ("tool-args", Arg::Lit(&args)),
            ("tool-git", Arg::Lit(if git { "1" } else { "" })),
        ],
    )?;
    eval_then_catching(ws, &format!("caos-tools/{name}"), Arg::Hash(&me))
}

/// STAGE TWO of a tree-tool call: `--result` is the tool's ArgTree, straight
/// from its `.caos-expr` — its worker image, its script and its `help`. Curry
/// the model's args onto it and run it over the workspace.
///
/// **This is the same ArgTree `caos-cli run-tool <name>` builds** (SPEC,
/// "Tools": two callers, one contract), so an agent's call and a hand-run share
/// one cache entry. `--in` is not curried here: `run-then` passes the input it
/// ran over, which is the workspace.
///
/// A `@git` tool additionally gets the workspace commit (`wc`, a gitlink it
/// reads as the raw commit object — history's entry point via `caos get-hash`)
/// and the turn's ref snapshot (`refs`, the same `name <hash>` lines the merge
/// tool resolves `--theirs` against). Only `@git` tools get them: `wc` moves
/// every step, so binding it into build/test would sink their caches.
fn launch_evaluated_tool(
    cfg: &Config,
    name: &str,
    ws: &str,
    wc: &str,
    run: &str,
    round: u64,
    base_head: &str,
    id: &str,
) -> Result<(), String> {
    let tool_tree = cas_hash(&arg("result"))?;
    let raw = read_arg("tool-args")?;
    let parsed: Value =
        serde_json::from_str(&raw).map_err(|e| format!("re-reading the tool's args: {e}"))?;
    let bound: Vec<(String, String)> = parsed
        .as_object()
        .ok_or("tool-args is not a JSON object")?
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
        .collect();
    let git = read_arg_opt("tool-git")?.is_some_and(|v| v == "1");

    // The model's arg values bind as LITERALS, so they land at
    // /cas/args/<name> for the script to read and — being part of the args
    // tree — key the run: the same tool called with a different hash is a
    // different job, not a cache hit.
    let mut kvs: Vec<(&str, Arg)> = bound
        .iter()
        .map(|(k, v)| (k.as_str(), Arg::Lit(v)))
        .collect();

    // History context, for `@git` tools only. `wc` is commit-kinded, so it
    // curries as a gitlink (`:commit=`) and the tool reads it as the raw commit
    // object — exactly how the merge tool receives `ours`.
    if git {
        kvs.push(("wc", Arg::Path(wc)));
        if let Some(refs) = cfg.merge_refs.as_deref() {
            kvs.push(("refs", Arg::Lit(refs)));
        }
    }
    let curried = caos_curry(Arg::Hash(&tool_tree), &kvs)?;
    let me = self_curry(
        wc,
        run,
        round,
        base_head,
        id,
        &[("current-tool", Arg::Lit(name)), ("ws", Arg::Path(ws))],
    )?;
    run_then_catching(ws, Arg::Hash(&curried), Arg::Hash(&me))
}

/// Launch a built-in history tool (`log`/`show`/`diff`): assemble its embedded
/// script (`githist::script`), `caos put` it into CAS, and curry it onto the
/// tools image with the `@git` context. Its result is rendered by the same
/// callback arm a project tool's is.
///
/// It does NOT go through [`launch_tree_tool`], and the difference is real: a
/// project tool is a directory in the workspace whose `.caos-expr` says what it
/// runs on, so reaching it means evaluating. This script ships with the harness
/// and has no expression — the image is `tools_image`, handed in as config —
/// so there is nothing to evaluate and no reason to spend a continuation.
#[allow(clippy::too_many_arguments)]
fn launch_githist(
    cfg: &Config,
    call: &Value,
    name: &str,
    bound: &[(String, String)],
    ws: &str,
    wc: &str,
    run: &str,
    round: u64,
    base_head: &str,
) -> Result<(), String> {
    let id = call["id"]
        .as_str()
        .ok_or("tool_use block has no string id")?;
    let image = cfg
        .tools_image
        .as_ref()
        .ok_or("launch_githist without a tools_image (drive guards this)")?;
    let body = githist::script(name).ok_or_else(|| format!("no built-in script for {name}"))?;
    let dir = scratch(&format!("githist-{name}"))?;
    let file = dir.join("worker.sh");
    fs::write(&file, body).map_err(|e| format!("writing {name} script: {e}"))?;
    let script = fresh("githist-script");
    caos(["put", path(&file), &script])?;

    let mut kvs: Vec<(&str, Arg)> = vec![("worker1", Arg::Path(&script))];
    kvs.extend(bound.iter().map(|(k, v)| (k.as_str(), Arg::Lit(v))));
    // History context: `wc` is commit-kinded, so it curries as a gitlink
    // (`:commit=`) and the tool reads it as the raw commit object — exactly how
    // the merge tool receives `ours`.
    kvs.push(("wc", Arg::Path(wc)));
    if let Some(refs) = cfg.merge_refs.as_deref() {
        kvs.push(("refs", Arg::Lit(refs)));
    }
    let curried = caos_curry(Arg::Hash(image), &kvs)?;
    let me = self_curry(
        wc,
        run,
        round,
        base_head,
        id,
        &[("current-tool", Arg::Lit(name)), ("ws", Arg::Path(ws))],
    )?;
    run_then_catching(ws, Arg::Hash(&curried), Arg::Hash(&me))
}

/// Rebuild ourselves as the callback for one compute tool. We carry our WHOLE current ArgTree
/// forward with [`own_args_tree`] (so the static config — `system`,
/// `bash-image`, `head`, `worker1`, and the optional
/// `model`/`base-url`/`conversation`/`grep-image`/`tools-image`/`merge-image`/`merge-refs`
/// — rides along and a NEW config arg needs no edit here) and manage only the
/// args this loop OWNS: unbind whichever are bound right now, then rebind the
/// ones that continue. Commit-valued paths (`head`, `wc`) ride as
/// gitlinks. Contrast the old approach — rebuild from the bare base and re-list
/// every arg — whose keep-list dropped a config arg the moment you forgot to add
/// it; here a forgotten carry is impossible and a forgotten unbind is a loud
/// rebind error, not a stale value.
fn self_curry(
    wc: &str,
    run: &str,
    round: u64,
    base_head: &str,
    current_id: &str,
    extras: &[(&str, Arg)],
) -> Result<String, String> {
    let round = round.to_string();

    // The loop/per-call args this function owns. Unbind whichever are bound in
    // the current invocation (so they neither double-bind nor persist), then
    // rebind the state below and the per-tool `extras`; `in`/`result` (run-then's
    // call args) are dropped — the server supplies fresh ones next call.
    //   loop state (rebound below): wc, run, round, base-head, current-id
    //   per-tool (rebound via `extras`): current-tool, ws, scope
    //   run-then's call args (dropped): in, result
    const MANAGED: &[&str] = &[
        "wc",
        "run",
        "round",
        "base-head",
        "current-id",
        "current-tool",
        "ws",
        "scope",
        // The two-stage tree-tool call: `tool-eval` marks the eval callback,
        // `tool-args`/`tool-git` carry what stage two needs to curry. Unbound
        // each hop like the rest, so stage two does not leak into the next call.
        "tool-eval",
        "tool-args",
        "tool-git",
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
        // The workspace commit the callback continues from (a gitlink).
        ("wc", Arg::Path(wc)),
        ("run", Arg::Lit(run)),
        ("round", Arg::Lit(&round)),
        ("base-head", Arg::Lit(base_head)),
        ("current-id", Arg::Lit(current_id)),
    ];
    for (name, value) in extras {
        kvs.push((name, *value));
    }
    caos_recurry(Arg::Hash(&own_args_tree()?), &unbind, &kvs)
}

// ---------------------------------------------------------------------------
// Transcript reconstruction (see design/agent-harness.md, "Commit structure").
// ---------------------------------------------------------------------------

fn conversation(cfg: &Config) -> Result<&str, String> {
    cfg.conversation
        .as_deref()
        .ok_or_else(|| "llm-step requires --conversation".to_string())
}

fn active_request(log: &progress::ConversationLog) -> Result<String, String> {
    let mut status = None;
    let mut request = None;
    for event in &log.events {
        if let Some(value) = event.value.get("status") {
            status = value.as_str().map(str::to_string);
        }
        if let Some(value) = event.value.get("request") {
            request = value.as_str().map(str::to_string);
        }
    }
    if !matches!(status.as_deref(), Some("queued" | "running")) {
        return Err(format!(
            "conversation is not active (status {})",
            status.as_deref().unwrap_or("unset")
        ));
    }
    let request = request.ok_or_else(|| "active conversation has no request hash".to_string())?;
    validate_run_hash(&request)?;
    Ok(request)
}

fn request_owner<'a>(
    log: &'a progress::ConversationLog,
    head: &str,
) -> Result<&'a str, String> {
    log.events
        .iter()
        .find(|event| event.commit == head)
        .and_then(|event| event.value.get("username"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("request head {head} has no human owner"))
}

#[derive(Debug, PartialEq, Eq)]
enum RequestStart {
    Running,
    Claim { expected: String, tree: String },
}

fn request_start_disposition(
    log: &progress::ConversationLog,
    run: &str,
    head: &str,
) -> Result<RequestStart, String> {
    let position = log
        .events
        .iter()
        .position(|event| event.commit == head)
        .ok_or_else(|| format!("request head {head} is not on the conversation event spine"))?;

    let recorded = request_after_head(log, position)?;
    let recorded = recorded.ok_or_else(|| format!("request head {head} has no admission event"))?;
    if recorded != run {
        return Err(format!(
            "request head {head} already belongs to request {recorded}, not {run}"
        ));
    }
    // A completed request is idempotent even if a later request is now queued.
    // The caller rereads this exact terminal result instead of letting the old
    // worker reclaim against the newer conversation status.
    if terminal_for_run(log, run)?.is_some() {
        return Ok(RequestStart::Running);
    }

    let active = active_request(log)?;
    if active != run {
        return Err(format!(
            "request {run} is stale; the conversation's active request is {active}"
        ));
    }
    let latest_status = log.events[position..]
        .iter()
        .filter(|event| event.value.get("request").and_then(Value::as_str) == Some(run))
        .filter_map(|event| event.value.get("status"))
        .last()
        .and_then(Value::as_str);
    match latest_status {
        Some("running") => return Ok(RequestStart::Running),
        Some("queued") => {}
        other => {
            return Err(format!(
                "queued request head {head} is no longer current (status {})",
                other.unwrap_or("invalid")
            ))
        }
    }
    let tree = log
        .events
        .last()
        .map(|event| event.tree.clone())
        .ok_or("conversation has no events")?;
    Ok(RequestStart::Claim {
        expected: log.head.clone(),
        tree,
    })
}

fn request_after_head(
    log: &progress::ConversationLog,
    position: usize,
) -> Result<Option<String>, String> {
    let request_event = log
        .events
        .get(position + 1)
        .filter(|event| event.value.get("request").and_then(Value::as_str).is_some());
    let request = request_event
        .and_then(|event| event.value.get("request"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(request) = &request {
        validate_run_hash(request)?;
    }
    if let Some(request_event) = request_event {
        let recorded_head = request_event
            .value
            .get("request_head")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "request event after {} has no request_head",
                    log.events[position].commit
                )
            })?;
        validate_run_hash(recorded_head)?;
        let expected = &log.events[position].commit;
        if recorded_head != expected {
            return Err(format!(
                "request event records head {recorded_head}, but follows request head {expected}"
            ));
        }
    }
    Ok(request)
}

fn request_for_head(log: &progress::ConversationLog, head: &str) -> Result<String, String> {
    let position = log
        .events
        .iter()
        .position(|event| event.commit == head)
        .ok_or_else(|| format!("request head {head} is not on the conversation event spine"))?;
    let request = request_after_head(log, position)?
        .ok_or_else(|| format!("request head {head} is not followed by a request event"))?;
    Ok(request)
}

fn validate_run_hash(run: &str) -> Result<(), String> {
    validate_hash(run, "conversation run")
}

pub(crate) fn validate_hash(hash: &str, what: &str) -> Result<(), String> {
    if hash.len() != 40
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(format!(
            "{what} must be a lowercase 40-character hexadecimal hash, got {hash:?}"
        ));
    }
    Ok(())
}

#[derive(Clone)]
struct RoundState {
    round: u64,
    calls: Vec<Value>,
    pending: Vec<Value>,
}

fn event_request_round(event: &progress::ConversationEvent) -> Result<(&str, u64), String> {
    let run = event
        .value
        .get("request")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("event {} has no string request", event.commit))?;
    validate_run_hash(run)?;
    let round = event
        .value
        .get("round")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("event {} has no integer round", event.commit))?;
    Ok((run, round))
}

fn latest_round(log: &progress::ConversationLog, run: &str) -> Result<Option<RoundState>, String> {
    let mut response_index = None;
    let mut round = 0;
    let mut calls = Vec::new();
    for (index, event) in log.events.iter().enumerate() {
        if event.value.get("response").is_none() {
            continue;
        }
        let (event_run, event_round) = event_request_round(event)?;
        if event_run != run {
            continue;
        }
        if response_index.is_some() && event_round <= round {
            return Err(format!(
                "run {run} has non-increasing response round {event_round}"
            ));
        }
        let response = event.value["response"]
            .as_array()
            .ok_or_else(|| format!("response event {} is not an array", event.commit))?;
        calls = response
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
            .cloned()
            .collect();
        response_index = Some(index);
        round = event_round;
    }
    let Some(response_index) = response_index else {
        return Ok(None);
    };

    let mut answered = std::collections::HashSet::new();
    for event in &log.events[response_index + 1..] {
        let Some(result) = event.value.get("result") else {
            continue;
        };
        let (event_run, event_round) = event_request_round(event)?;
        if event_run != run || event_round != round {
            continue;
        }
        let id = result
            .get("tool_use_id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("result event {} has no tool_use_id", event.commit))?;
        if !answered.insert(id.to_string()) {
            return Err(format!(
                "run {run} round {round} has duplicate result for call {id}"
            ));
        }
    }
    let mut seen_calls = std::collections::HashSet::new();
    for call in &calls {
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .ok_or("tool_use block has no string id")?;
        if !seen_calls.insert(id) {
            return Err(format!("run {run} round {round} repeats call id {id}"));
        }
    }
    if let Some(unexpected) = answered.iter().find(|id| !seen_calls.contains(id.as_str())) {
        return Err(format!(
            "run {run} round {round} has a result for unknown call {unexpected}"
        ));
    }
    let pending = calls
        .iter()
        .filter(|call| {
            call.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !answered.contains(id))
        })
        .cloned()
        .collect();
    Ok(Some(RoundState {
        round,
        calls,
        pending,
    }))
}

fn terminal_for_run<'a>(
    log: &'a progress::ConversationLog,
    run: &str,
) -> Result<Option<&'a progress::ConversationEvent>, String> {
    for event in log.events.iter().rev() {
        if event.value.get("request").and_then(Value::as_str) != Some(run) {
            continue;
        }
        if matches!(
            event.value.get("status").and_then(Value::as_str),
            Some("idle" | "failed")
        ) {
            return Ok(Some(event));
        }
    }
    Ok(None)
}

fn escaped(events: &[progress::ConversationEvent], run: &str) -> Result<bool, String> {
    for event in events {
        let Some(escape) = event.value.get("escape") else {
            continue;
        };
        let request = escape
            .get("request")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Escape event {} has no string request", event.commit))?;
        validate_run_hash(request)?;
        if request == run {
            return Ok(true);
        }
    }
    Ok(false)
}

fn escaped_for_run(log: &progress::ConversationLog, run: &str) -> Result<bool, String> {
    escaped(&log.events, run)
}

/// Inspect exactly the commits that displaced an attempted append. The
/// expected head must remain on the canonical spine; only its suffix can
/// interrupt work produced against that head.
fn escaped_after(
    log: &progress::ConversationLog,
    expected_head: &str,
    run: &str,
) -> Result<bool, String> {
    let position = log
        .events
        .iter()
        .position(|event| event.commit == expected_head)
        .ok_or_else(|| format!("lost append base {expected_head} is not on the event spine"))?;
    escaped(&log.events[position + 1..], run)
}

fn finish_interrupted(
    cfg: &Config,
    run: &str,
    initial: progress::ConversationLog,
) -> Result<(), String> {
    let conversation = conversation(cfg)?;
    let mut log = initial;
    for _ in 0..32 {
        if let Some(terminal) = terminal_for_run(&log, run)? {
            return finish_from_terminal(cfg, &log, terminal);
        }
        if active_request(&log).as_deref() != Ok(run) {
            return Err(format!("Escape belongs to stale request {run}"));
        }
        if let Some(state) = latest_round(&log, run)? {
            if let Some(call) = state.pending.first() {
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("pending tool call has no id")?;
                let result = json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "is_error": true,
                    "content": [{"type": "text", "text":
                        "interrupted before this tool ran"}],
                });
                let tree = log
                    .events
                    .last()
                    .map(|event| event.tree.clone())
                    .ok_or("conversation has no events")?;
                append_tool_result(cfg, run, state.round, &log.head, &result, &tree, None)?;
                log = progress::conversation_log(conversation)?;
                continue;
            }
        }
        let tree = log
            .events
            .last()
            .map(|event| event.tree.as_str())
            .ok_or("conversation has no events")?;
        let round = latest_round(&log, run)?.map_or(0, |state| state.round);
        let event = json!({
            "request": run,
            "round": round,
            "status": "idle",
            "interrupted": true,
        });
        match progress::append_event_at_head(conversation, &log.head, &event, tree)? {
            progress::ConditionalAppend::Appended(_) => {
                let terminal = progress::conversation_log(conversation)?;
                let event = terminal_for_run(&terminal, run)?
                    .ok_or("interrupted request has no terminal event")?;
                return finish_from_terminal(cfg, &terminal, event);
            }
            progress::ConditionalAppend::HeadChanged(_) => {
                log = progress::conversation_log(conversation)?;
            }
        }
    }
    Err("conversation kept changing while finishing Escape".to_string())
}

/// An idle foreground terminal event is the successful worker's final recovery
/// point. Failed terminals return their error here and are reconciled by
/// `record_failure`, which also owns failures appended by this invocation.
fn finish_from_terminal(
    cfg: &Config,
    log: &progress::ConversationLog,
    event: &progress::ConversationEvent,
) -> Result<(), String> {
    match event.value.get("status").and_then(Value::as_str) {
        Some("idle") => {
            reconcile_async_tasks(cfg, log)?;
            forward_commit(&event.commit)
        }
        Some(status) => Err(event
            .value
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("conversation request ended {status}"))),
        None => Err(format!("terminal event {} has no status", event.commit)),
    }
}

fn forward_commit(hash: &str) -> Result<(), String> {
    let source = fresh("terminal");
    caos(["get-hash", hash, &source])?;
    forward(&source, "/cas/out")
}

fn canonical_workspace(log: &progress::ConversationLog) -> Result<(String, String), String> {
    let tree = log
        .events
        .last()
        .map(|event| event.tree.as_str())
        .ok_or("conversation has no events")?;
    let ws = fresh("ws-canonical");
    caos(["get-hash", tree, &ws])?;
    let wc = fresh("wc-canonical");
    caos(["get-hash", &log.head, &wc])?;
    Ok((ws, wc))
}

struct ReloadedPending {
    head: String,
    /// `Some(tree)` means the pending record is still absent and must be
    /// rebuilt on this head. `None` means another writer already recorded a
    /// durable state for the task.
    workspace: Option<String>,
}

fn retry_pending_append<A, R>(
    initial_head: &str,
    initial_workspace: &str,
    mut append: A,
    mut reload: R,
) -> Result<String, String>
where
    A: FnMut(&str, &str) -> Result<progress::ConditionalAppend, String>,
    R: FnMut() -> Result<ReloadedPending, String>,
{
    let mut head = initial_head.to_string();
    let mut workspace = initial_workspace.to_string();
    for _ in 0..32 {
        match append(&head, &workspace)? {
            progress::ConditionalAppend::Appended(result) => return Ok(result.commit),
            progress::ConditionalAppend::HeadChanged(_) => {
                let reloaded = reload()?;
                head = reloaded.head;
                let Some(reloaded_workspace) = reloaded.workspace else {
                    return Ok(head);
                };
                workspace = reloaded_workspace;
            }
        }
    }
    Err("conversation kept changing while recording async task pending".to_string())
}

fn ensure_async_status(cfg: &Config, task: &str) -> Result<async_work::TaskState, String> {
    validate_run_hash(task)?;
    let conversation = conversation(cfg)?;
    let log = progress::conversation_log(conversation)?;
    if let Some(state) = async_work::task_status(log.events.iter().map(|event| &event.value), task)?
    {
        return Ok(state);
    }
    let tree = log
        .events
        .last()
        .map(|event| event.tree.clone())
        .ok_or("conversation has no events")?;
    let event = json!({"async": {"task": task, "status": "pending"}});
    let mut observed_state = None;
    retry_pending_append(
        &log.head,
        &tree,
        |head, workspace| progress::append_event_at_head(conversation, head, &event, workspace),
        || {
            let log = progress::conversation_log(conversation)?;
            let state = async_work::task_status(log.events.iter().map(|event| &event.value), task)?;
            if let Some(state) = state {
                observed_state = Some(state);
                return Ok(ReloadedPending {
                    head: log.head,
                    workspace: None,
                });
            }
            let workspace = log
                .events
                .last()
                .map(|event| event.tree.clone())
                .ok_or("conversation has no events")?;
            Ok(ReloadedPending {
                head: log.head,
                workspace: Some(workspace),
            })
        },
    )?;
    Ok(observed_state.unwrap_or_else(|| async_work::TaskState {
        task: task.to_string(),
        status: "pending".to_string(),
        result: None,
        event_index: log.events.len(),
    }))
}

/// Re-admit pending Qs. A terminal event already carries its addressable result
/// object id, so it is complete without consulting a separate result ref.
/// Failures are warnings: the durable pending state remains on F and a later
/// invocation retries the same Q. Validation inside `readmit_task` prevents a
/// forged event from targeting another ref.
fn reconcile_async_tasks(cfg: &Config, log: &progress::ConversationLog) -> Result<(), String> {
    let tasks = async_work::tasks(log.events.iter().map(|event| &event.value));
    if tasks.is_empty() {
        return Ok(());
    }
    let conversation = conversation(cfg)?;
    for state in tasks {
        if state.status != "pending" {
            continue;
        }
        if let Err(error) = async_work::readmit_task(&state.task, conversation) {
            eprintln!(
                "llm-step: could not re-admit async task {} ({}): {error}",
                state.task, state.status
            );
        }
    }
    Ok(())
}

fn resume_run(
    cfg: &Config,
    run: &str,
    head_hash: &str,
    log: progress::ConversationLog,
) -> Result<(), String> {
    if let Some(terminal) = terminal_for_run(&log, run)? {
        return finish_from_terminal(cfg, &log, terminal);
    }
    if escaped_for_run(&log, run)? {
        return finish_interrupted(cfg, run, log);
    }
    let active = active_request(&log)?;
    if active != run || request_for_head(&log, head_hash).as_deref() != Ok(run) {
        return Err(format!(
            "request {run} is stale; conversation is running {active}"
        ));
    }
    let (ws, wc) = canonical_workspace(&log)?;
    if let Some(state) = latest_round(&log, run)? {
        if !state.pending.is_empty() {
            let base_head = log.head.clone();
            return drive(
                cfg,
                ws,
                wc,
                head_hash,
                &state.pending,
                run,
                state.round,
                base_head,
            );
        }
        let messages = event_messages(&log, true)?;
        let prev = log.head.clone();
        let next_round = state
            .round
            .checked_add(1)
            .ok_or_else(|| format!("run {run} exhausted its round counter"))?;
        return llm_round(cfg, messages, &ws, head_hash, &prev, run, next_round);
    }
    let messages = event_messages(&log, true)?;
    let prev = log.head.clone();
    llm_round(cfg, messages, &ws, head_hash, &prev, run, 0)
}

/// Rebuild the Anthropic transcript from events. User interjections that land
/// while a tool batch is outstanding are held until all of that response's
/// results can be emitted together, in call order, immediately after the
/// assistant tool-use message. Terminal async-task notices are synthesized at
/// their durable event's position on every rebuild, so observing one extends
/// the transcript permanently instead of moving or disappearing next round.
fn event_messages(
    log: &progress::ConversationLog,
    include_async_status: bool,
) -> Result<Vec<Value>, String> {
    struct ToolBatch {
        run: String,
        round: u64,
        ids: Vec<String>,
        results: std::collections::HashMap<String, Value>,
        deferred_users: Vec<Value>,
    }

    fn flush_batch(messages: &mut Vec<Value>, batch: &mut Option<ToolBatch>) -> Result<(), String> {
        let Some(current) = batch.take() else {
            return Ok(());
        };
        if current.results.len() != current.ids.len() {
            *batch = Some(current);
            return Ok(());
        }
        let results = current
            .ids
            .iter()
            .map(|id| {
                current
                    .results
                    .get(id)
                    .cloned()
                    .ok_or_else(|| format!("missing result for call {id}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        messages.push(message("user", Value::Array(results)));
        messages.extend(current.deferred_users);
        Ok(())
    }

    fn push_user(messages: &mut Vec<Value>, batch: &mut Option<ToolBatch>, user: Value) {
        if let Some(current) = batch.as_mut() {
            current.deferred_users.push(user);
        } else {
            messages.push(user);
        }
    }

    let async_notices = if include_async_status {
        async_work::tasks(log.events.iter().map(|event| &event.value))
            .into_iter()
            .filter(|state| matches!(state.status.as_str(), "complete" | "failed"))
            .map(|state| {
                let result = state
                    .result
                    .as_deref()
                    .expect("validated terminal async state has a result");
                let notice = user_text(&format!(
                    "Independent task {} is {}. Its result is {}.",
                    state.task, state.status, result
                ));
                (state.event_index, notice)
            })
            .collect::<std::collections::HashMap<_, _>>()
    } else {
        std::collections::HashMap::new()
    };

    let mut messages = Vec::new();
    let mut batch: Option<ToolBatch> = None;
    for (event_index, event) in log.events.iter().enumerate() {
        if let Some(response) = event.value.get("response") {
            if batch.is_some() {
                flush_batch(&mut messages, &mut batch)?;
                if batch.is_some() {
                    return Err(format!(
                        "response event {} arrived before the preceding tool batch completed",
                        event.commit
                    ));
                }
            }
            let blocks = response.as_array().ok_or_else(|| {
                format!(
                    "conversation response event {} is not an array",
                    event.commit
                )
            })?;
            messages.push(message("assistant", Value::Array(blocks.clone())));
            let ids = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
                .map(|call| {
                    call.get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| {
                            format!("response event {} has a call without an id", event.commit)
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if !ids.is_empty() {
                let (run, round) = event_request_round(event)?;
                batch = Some(ToolBatch {
                    run: run.to_string(),
                    round,
                    ids,
                    results: std::collections::HashMap::new(),
                    deferred_users: Vec::new(),
                });
            }
        }
        if let Some(result) = event.value.get("result") {
            let (run, round) = event_request_round(event)?;
            let current = batch.as_mut().ok_or_else(|| {
                format!(
                    "result event {} has no preceding tool response",
                    event.commit
                )
            })?;
            if current.run != run || current.round != round {
                return Err(format!(
                    "result event {} belongs to {run}/{round}, expected {}/{}",
                    event.commit, current.run, current.round
                ));
            }
            let id = result
                .get("tool_use_id")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("result event {} has no tool_use_id", event.commit))?;
            if !current.ids.iter().any(|expected| expected == id) {
                return Err(format!(
                    "result event {} names unknown call {id}",
                    event.commit
                ));
            }
            if current
                .results
                .insert(id.to_string(), result.clone())
                .is_some()
            {
                return Err(format!(
                    "result event {} duplicates call {id}",
                    event.commit
                ));
            }
            flush_batch(&mut messages, &mut batch)?;
        }
        if event.value.get("author").and_then(Value::as_str) == Some("user") {
            let content = event
                .value
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("user event {} has no string content", event.commit))?;
            push_user(&mut messages, &mut batch, user_text(content));
        }
        if let Some(notice) = async_notices.get(&event_index) {
            push_user(&mut messages, &mut batch, notice.clone());
        }
    }
    flush_batch(&mut messages, &mut batch)?;
    Ok(messages)
}

fn append_tool_result(
    cfg: &Config,
    run: &str,
    round: u64,
    base_head: &str,
    result: &Value,
    tree: &str,
    extra_parent: Option<&str>,
) -> Result<(), String> {
    let call_id = result
        .get("tool_use_id")
        .and_then(Value::as_str)
        .ok_or("tool result block has no tool_use_id")?;
    for _ in 0..32 {
        let current = progress::conversation_log(conversation(cfg)?)?;
        if let Some(existing) = current.events.iter().find(|event| {
            event.value.get("request").and_then(Value::as_str) == Some(run)
                && event.value.get("round").and_then(Value::as_u64) == Some(round)
                && event
                    .value
                    .get("result")
                    .and_then(|value| value.get("tool_use_id"))
                    .and_then(Value::as_str)
                    == Some(call_id)
        }) {
            if existing.value.get("result") != Some(result) {
                return Err(format!(
                    "call {call_id} in run {run} round {round} has conflicting results"
                ));
            }
            return Ok(());
        }
        if terminal_for_run(&current, run)?.is_some()
            || active_request(&current).as_deref() != Ok(run)
        {
            return Err(format!("result for stale request {run}, call {call_id}"));
        }
        let state = latest_round(&current, run)?
            .ok_or_else(|| format!("result for run {run} before any model response"))?;
        if state.round != round
            || !state
                .calls
                .iter()
                .any(|call| call.get("id").and_then(Value::as_str) == Some(call_id))
        {
            return Err(format!(
                "call {call_id} is not pending in run {run} round {round}"
            ));
        }
        let base_tree = current
            .events
            .iter()
            .find(|event| event.commit == base_head)
            .map(|event| event.tree.as_str())
            .ok_or_else(|| format!("tool base {base_head} is not on the event spine"))?;
        let current_tree = current
            .events
            .last()
            .map(|event| event.tree.as_str())
            .ok_or("conversation has no events")?;
        let merged_tree = match progress::retry_tree(base_tree, tree, current_tree)? {
            progress::RetryTree::Merged(tree) => tree,
            progress::RetryTree::Conflict(conflict) => {
                let proposal = extra_parent.ok_or_else(|| {
                    format!(
                        "workspace conflict for call {call_id} has no proposal commit to retain"
                    )
                })?;
                let error = format!(
                    "workspace proposal for call {call_id} conflicted with concurrent conversation changes: {}",
                    conflict.detail
                );
                let conflict_result = json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "is_error": true,
                    "content": [{"type": "text", "text": error}],
                });
                let event = json!({
                    "request": run,
                    "round": round,
                    "status": "failed",
                    "error": error,
                    "result": conflict_result,
                    "workspace_conflict": {
                        "base": base_head,
                        "base_tree": base_tree,
                        "current": current.head,
                        "current_tree": current_tree,
                        "proposal": proposal,
                        "proposal_tree": tree,
                        "paths": conflict.paths,
                    },
                });
                match progress::append_event_at_head_with_parent(
                    conversation(cfg)?,
                    &current.head,
                    &event,
                    current_tree,
                    Some(proposal),
                )? {
                    progress::ConditionalAppend::Appended(appended) => {
                        return Err(format!(
                            "{error}; conflicting proposal recorded at {}",
                            appended.commit
                        ))
                    }
                    progress::ConditionalAppend::HeadChanged(_) => continue,
                }
            }
        };
        let event = json!({"request": run, "round": round, "result": result});
        match progress::append_event_at_head_with_parent(
            conversation(cfg)?,
            &current.head,
            &event,
            &merged_tree,
            extra_parent,
        )? {
            progress::ConditionalAppend::Appended(_) => return Ok(()),
            progress::ConditionalAppend::HeadChanged(_) => continue,
        }
    }
    Err(format!(
        "conversation kept changing while recording call {call_id}"
    ))
}

fn record_failure_with<L, P, A, R>(
    run: &str,
    error: &str,
    mut load: L,
    mut append_pending: P,
    mut append_terminal: A,
    mut reconcile: R,
) -> Result<(), String>
where
    L: FnMut() -> Result<progress::ConversationLog, String>,
    P: FnMut(u64, &str, &Value, &str) -> Result<(), String>,
    A: FnMut(&str, &Value, &str) -> Result<progress::ConditionalAppend, String>,
    R: FnMut(&progress::ConversationLog) -> Result<(), String>,
{
    for _ in 0..32 {
        let log = load()?;
        if terminal_for_run(&log, run)?.is_some() {
            // A retry can arrive after the foreground failure became durable
            // but before its terminal-boundary recovery ran.
            reconcile(&log)?;
            return Ok(());
        }
        if active_request(&log).as_deref() != Ok(run) {
            return Ok(());
        }
        if let Some(state) = latest_round(&log, run)? {
            if let Some(call) = state.pending.first() {
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("pending tool call has no id")?;
                let result = json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "is_error": true,
                    "content": [{"type": "text", "text": format!(
                        "the request stopped before this tool completed: {error}"
                    )}],
                });
                let tree = log
                    .events
                    .last()
                    .map(|event| event.tree.clone())
                    .ok_or("conversation has no events")?;
                append_pending(state.round, &log.head, &result, &tree)?;
                continue;
            }
        }
        let tree = log
            .events
            .last()
            .map(|event| event.tree.as_str())
            .ok_or("conversation has no events")?;
        let round = latest_round(&log, run)?.map_or(0, |state| state.round);
        let event = json!({
            "request": run,
            "round": round,
            "status": "failed",
            "error": error,
        });
        match append_terminal(&log.head, &event, tree)? {
            progress::ConditionalAppend::Appended(_) => {
                // This failure is the worker's final safe boundary. Reload so
                // tasks appended concurrently with the failure are included.
                let terminal = load()?;
                reconcile(&terminal)?;
                return Ok(());
            }
            progress::ConditionalAppend::HeadChanged(_) => continue,
        }
    }
    Err("conversation kept changing while recording request failure".to_string())
}

fn record_failure(cfg: &Config, error: &str) -> Result<(), String> {
    let conversation = conversation(cfg)?;
    let run = read_arg_opt("run")?.unwrap_or(own_args_tree()?);
    record_failure_with(
        &run,
        error,
        || progress::conversation_log(conversation),
        |round, base_head, result, tree| {
            append_tool_result(cfg, &run, round, base_head, result, tree, None)
        },
        |head, event, tree| progress::append_event_at_head(conversation, head, event, tree),
        |log| reconcile_async_tasks(cfg, log),
    )
}

// ---------------------------------------------------------------------------
// Blocks and small helpers.
// ---------------------------------------------------------------------------

/// The full tool registry: bash, grep and the cargo tools (the sub-run tools)
/// plus the inline file tools (`tools.rs`).
fn registry(cfg: &Config, ws: &str) -> Result<Vec<Value>, String> {
    let mut tools = vec![bash_tool()];
    tools.extend(tools::declarations());
    if cfg.run_and_update_ref_image.is_some() {
        tools.extend(subagents::declarations());
        tools.push(async_work::declaration());
    }
    if cfg.grep_image.is_some() {
        tools.push(tools::grep_declaration());
    }
    if cfg.merge_image.is_some() {
        tools.push(merge_tool());
    }
    // The built-in history tools (log/show/diff) ship with the harness and run
    // on the handed-in tools image, so they keep its gate.
    if cfg.tools_image.is_some() {
        tools.extend(githist::declarations());
    }
    // Tree tools: whatever `caos-tools/<name>/` directories the CURRENT
    // workspace carries — re-discovered every round, so the set tracks the
    // agent's own edits. No gate: a tool's `.caos-expr` names the image it runs
    // on, so the harness needs no image of its own to offer one.
    for tool in tools::tree_tools(ws)? {
        tools.push(tools::tree_tool_declaration(&tool));
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

fn record_interrupted_response(
    cfg: &Config,
    run: &str,
    round: u64,
    blocks: &[Value],
    calls: Option<&[Value]>,
    mut log: progress::ConversationLog,
) -> Result<(), String> {
    let conversation = conversation(cfg)?;
    let response = Value::Array(blocks.to_vec());
    for _ in 0..32 {
        if let Some(terminal) = terminal_for_run(&log, run)? {
            return finish_from_terminal(cfg, &log, terminal);
        }
        if active_request(&log).as_deref() != Ok(run) {
            return Err(format!("Escape belongs to stale request {run}"));
        }
        if let Some(existing) = log.events.iter().find(|event| {
            event.value.get("request").and_then(Value::as_str) == Some(run)
                && event.value.get("round").and_then(Value::as_u64) == Some(round)
                && event.value.get("response").is_some()
        }) {
            if existing.value.get("response") != Some(&response) {
                return Err(format!(
                    "run {run} round {round} already has a different model response"
                ));
            }
            return finish_interrupted(cfg, run, log);
        }
        let tree = log
            .events
            .last()
            .map(|event| event.tree.as_str())
            .ok_or("conversation has no events")?;
        let event = match calls {
            Some(calls) => json!({
                "request": run,
                "round": round,
                "author": "assistant",
                "content": response_text(blocks),
                "response": blocks,
                "calls": calls,
            }),
            None => json!({
                "request": run,
                "round": round,
                "author": "assistant",
                "content": response_text(blocks),
                "response": blocks,
                "status": "idle",
                "interrupted": true,
            }),
        };
        match progress::append_event_at_head(conversation, &log.head, &event, tree)? {
            progress::ConditionalAppend::Appended(_) => {
                return finish_interrupted(
                    cfg,
                    run,
                    progress::conversation_log(conversation)?,
                )
            }
            progress::ConditionalAppend::HeadChanged(_) => {
                log = progress::conversation_log(conversation)?;
            }
        }
    }
    Err("conversation kept changing while recording an interrupted response".to_string())
}

/// Validate the relationship between a response's stop reason and tool blocks
/// before the response is recorded. Only `tool_use` may carry calls; an
/// `end_turn` with calls would be replayed as a batch that can never receive
/// results.
fn validated_tool_calls(
    stop_reason: &str,
    tool_uses: &[Value],
) -> Result<Option<Vec<Value>>, String> {
    match stop_reason {
        "tool_use" => durable_tool_calls(tool_uses).map(Some),
        "end_turn" if !tool_uses.is_empty() => {
            Err("stop_reason end_turn but response contains tool_use blocks".to_string())
        }
        _ => Ok(None),
    }
}

/// Build the durable call projection only after proving the response can be
/// folded again. IDs are scoped to this response's request/round, so reuse by
/// a later round is valid; duplicates inside one response are not.
fn durable_tool_calls(tool_uses: &[Value]) -> Result<Vec<Value>, String> {
    if tool_uses.is_empty() {
        return Err("stop_reason tool_use but no tool_use blocks".to_string());
    }
    let mut ids = std::collections::HashSet::new();
    tool_uses
        .iter()
        .enumerate()
        .map(|(index, call)| {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("model tool_use block {index} has no string id"))?;
            if !ids.insert(id) {
                return Err(format!("model response repeats tool_use id {id:?}"));
            }
            Ok(json!({
                "id": id,
                "name": call.get("name").cloned().unwrap_or(Value::Null),
                "args": call.get("input").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect()
}

/// A fresh, unique direct-child CAS path (CAS paths are single-assignment).
pub(crate) fn fresh(prefix: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("/cas/{prefix}-{n}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_timestamp_comes_from_the_committer() {
        let commit = "tree 0123456789012345678901234567890123456789\n\
author user <user@example.com> 1700000000 +0000\n\
committer agent <agent@example.com> 1700000123 +0000\n\nmessage\n";
        assert_eq!(parse_commit_timestamp(commit).unwrap(), 1_700_000_123);
    }

    #[test]
    fn durable_hashes_are_canonical_lowercase() {
        assert!(validate_hash(&"a".repeat(40), "test hash").is_ok());
        assert!(validate_hash(&"A".repeat(40), "test hash")
            .unwrap_err()
            .contains("lowercase"));
    }

    fn log(values: Vec<Value>) -> progress::ConversationLog {
        progress::ConversationLog {
            head: "f".repeat(40),
            events: values
                .into_iter()
                .enumerate()
                .map(|(index, value)| progress::ConversationEvent {
                    commit: format!("{index:040x}"),
                    tree: "a".repeat(40),
                    value,
                })
                .collect(),
        }
    }

    #[test]
    fn canonical_log_tracks_pending_calls_by_request_and_round() {
        let run = "b".repeat(40);
        let call_a = json!({"type":"tool_use","id":"a","name":"read","input":{"path":"a"}});
        let call_b = json!({"type":"tool_use","id":"b","name":"read","input":{"path":"b"}});
        let history = log(vec![
            json!({"author":"user","content":"hello","status":"queued"}),
            json!({"request":run,"status":"running"}),
            json!({"request":run,"round":0,"response":[call_a.clone(),call_b.clone()],"calls":[]}),
            json!({"request":run,"round":0,"result":{"type":"tool_result","tool_use_id":"a","content":"one"}}),
        ]);

        assert_eq!(active_request(&history).unwrap(), run);
        let state = latest_round(&history, &run).unwrap().unwrap();
        assert_eq!(state.round, 0);
        assert_eq!(state.pending, [call_b]);
        // An incomplete tool batch is not sent back to Anthropic yet.
        assert_eq!(
            event_messages(&history, false).unwrap(),
            [
                json!({"role":"user","content":"hello"}),
                json!({"role":"assistant","content":[call_a, state.calls[1].clone()]})
            ]
        );
    }

    #[test]
    fn durable_tool_calls_reject_ids_that_would_brick_replay() {
        let valid = json!({"type":"tool_use","id":"same","name":"read","input":{}});
        assert_eq!(
            durable_tool_calls(std::slice::from_ref(&valid)).unwrap()[0]["id"],
            "same"
        );
        // The same model-local ID remains valid in a later invocation/round.
        assert!(durable_tool_calls(std::slice::from_ref(&valid)).is_ok());

        let missing = json!({"type":"tool_use","name":"read","input":{}});
        assert!(durable_tool_calls(&[missing])
            .unwrap_err()
            .contains("no string id"));
        let non_string = json!({"type":"tool_use","id":7,"name":"read","input":{}});
        assert!(durable_tool_calls(&[non_string])
            .unwrap_err()
            .contains("no string id"));
        assert!(durable_tool_calls(&[valid.clone(), valid])
            .unwrap_err()
            .contains("repeats tool_use id"));
    }

    #[test]
    fn terminal_response_rejects_tool_calls_before_recording() {
        let call = json!({"type":"tool_use","id":"call","name":"read","input":{}});
        assert!(validated_tool_calls("end_turn", &[]).unwrap().is_none());
        assert!(validated_tool_calls("end_turn", &[call.clone()])
            .unwrap_err()
            .contains("end_turn"));
        assert_eq!(
            validated_tool_calls("tool_use", &[call]).unwrap().unwrap()[0]["id"],
            "call"
        );
    }

    #[test]
    fn interjections_follow_a_complete_ordered_tool_result_batch() {
        let run = "b".repeat(40);
        let call_a = json!({"type":"tool_use","id":"a","name":"read","input":{}});
        let call_b = json!({"type":"tool_use","id":"b","name":"read","input":{}});
        let history = log(vec![
            json!({"author":"user","content":"start"}),
            json!({"request":run,"round":0,"response":[call_a.clone(),call_b.clone()]}),
            json!({"request":run,"round":0,"result":{"type":"tool_result","tool_use_id":"a","content":"one"}}),
            json!({"author":"user","content":"also do this"}),
            json!({"request":run,"round":0,"result":{"type":"tool_result","tool_use_id":"b","content":"two"}}),
        ]);
        assert_eq!(
            event_messages(&history, false).unwrap(),
            [
                json!({"role":"user","content":"start"}),
                json!({"role":"assistant","content":[call_a,call_b]}),
                json!({"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"a","content":"one"},
                    {"type":"tool_result","tool_use_id":"b","content":"two"}
                ]}),
                json!({"role":"user","content":"also do this"}),
            ]
        );
    }

    #[test]
    fn result_ids_are_scoped_to_their_round() {
        let run = "b".repeat(40);
        let call = json!({"type":"tool_use","id":"same","name":"read","input":{}});
        let result = json!({"type":"tool_result","tool_use_id":"same","content":"ok"});
        let history = log(vec![
            json!({"request":run,"round":0,"response":[call.clone()]}),
            json!({"request":run,"round":0,"result":result}),
            json!({"request":run,"round":1,"response":[call]}),
        ]);
        let state = latest_round(&history, &run).unwrap().unwrap();
        assert_eq!(state.round, 1);
        assert_eq!(state.pending.len(), 1);
    }

    #[test]
    fn terminal_async_notice_stays_in_the_prompt_prefix_after_a_response() {
        let run = "b".repeat(40);
        let task = "c".repeat(40);
        let result = "d".repeat(40);
        let mut events = vec![
            json!({"author":"user","content":"start"}),
            json!({"request":run,"round":0,"response":[{"type":"text","text":"working"}]}),
            json!({"async":{"task":task,"status":"complete","result":result}}),
        ];
        let before_response = event_messages(&log(events.clone()), true).unwrap();
        assert!(before_response.iter().any(|message| {
            message["content"]
                .as_str()
                .is_some_and(|text| text.contains("Independent task") && text.contains(&result))
        }));

        events.push(json!({
            "request":run,
            "round":1,
            "response":[{"type":"text","text":"observed"}]
        }));
        let after_response = event_messages(&log(events), true).unwrap();
        assert_eq!(
            &after_response[..before_response.len()],
            before_response.as_slice()
        );
        assert_eq!(
            after_response
                .iter()
                .filter(|message| {
                    message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("Independent task"))
                })
                .count(),
            1
        );
        assert_eq!(
            after_response.last(),
            Some(&json!({
                "role": "assistant",
                "content": [{"type":"text","text":"observed"}]
            }))
        );
    }

    #[test]
    fn terminal_is_identified_by_request() {
        let old = "a".repeat(40);
        let current = "b".repeat(40);
        let history = log(vec![
            json!({"request":old,"round":0,"status":"idle"}),
            json!({"request":current,"status":"running"}),
        ]);
        assert!(terminal_for_run(&history, &old).unwrap().is_some());
        assert!(terminal_for_run(&history, &current).unwrap().is_none());
    }

    #[test]
    fn failed_append_reads_only_its_new_escape_commits() {
        let old = "a".repeat(40);
        let current = "b".repeat(40);
        let history = log(vec![
            json!({"request":old,"status":"running"}),
            json!({"escape":{"request":old}}),
            json!({"escape":{"request":current}}),
        ]);
        assert!(escaped_for_run(&history, &old).unwrap());
        assert!(escaped_for_run(&history, &current).unwrap());
        assert!(escaped_after(&history, &format!("{:040x}", 0), &old).unwrap());
        assert!(escaped_after(&history, &format!("{:040x}", 0), &current).unwrap());
        assert!(!escaped_after(&history, &format!("{:040x}", 1), &old).unwrap());
        assert!(escaped_after(&history, &format!("{:040x}", 1), &current).unwrap());
    }

    #[test]
    fn preexisting_failed_terminal_reconciles_before_failure_recording_returns() {
        let run = "b".repeat(40);
        let task = "c".repeat(40);
        let history = log(vec![
            json!({"request":run,"status":"running"}),
            json!({"async":{"task":task,"status":"pending"}}),
            json!({"request":run,"round":0,"status":"failed","error":"boom"}),
        ]);
        let reconciliations = std::cell::Cell::new(0);

        record_failure_with(
            &run,
            "boom",
            || Ok(history.clone()),
            |_, _, _, _| panic!("a terminal request cannot have a pending foreground call"),
            |_, _, _| panic!("an existing terminal must not be appended again"),
            |terminal| {
                reconciliations.set(reconciliations.get() + 1);
                let tasks = async_work::tasks(terminal.events.iter().map(|event| &event.value));
                assert_eq!(tasks.len(), 1);
                assert_eq!(tasks[0].task, task);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(reconciliations.get(), 1);
    }

    #[test]
    fn newly_appended_failed_terminal_reloads_then_reconciles() {
        let run = "b".repeat(40);
        let task = "c".repeat(40);
        let running = log(vec![
            json!({"request":run,"status":"running"}),
            json!({"async":{"task":task,"status":"pending"}}),
        ]);
        let terminal = log(vec![
            json!({"request":run,"status":"running"}),
            json!({"async":{"task":task,"status":"pending"}}),
            json!({"request":run,"round":0,"status":"failed","error":"boom"}),
        ]);
        let mut loads = std::collections::VecDeque::from([running, terminal]);
        let appends = std::cell::Cell::new(0);
        let reconciliations = std::cell::Cell::new(0);

        record_failure_with(
            &run,
            "boom",
            || loads.pop_front().ok_or("unexpected extra load".to_string()),
            |_, _, _, _| panic!("the request has no pending foreground call"),
            |head, event, tree| {
                appends.set(appends.get() + 1);
                assert_eq!(head, "f".repeat(40));
                assert_eq!(tree, "a".repeat(40));
                assert_eq!(event["status"], "failed");
                Ok(progress::ConditionalAppend::Appended(
                    progress::AppendResult {
                        commit: "d".repeat(40),
                        previous_head: head.to_string(),
                        retries: 0,
                    },
                ))
            },
            |reloaded| {
                reconciliations.set(reconciliations.get() + 1);
                assert!(terminal_for_run(reloaded, &run)?.is_some());
                let tasks = async_work::tasks(reloaded.events.iter().map(|event| &event.value));
                assert_eq!(tasks.len(), 1);
                assert_eq!(tasks[0].task, task);
                Ok(())
            },
        )
        .unwrap();

        assert!(loads.is_empty());
        assert_eq!(appends.get(), 1);
        assert_eq!(reconciliations.get(), 1);
    }

    #[test]
    fn original_unadmitted_anchor_is_rejected() {
        let run = "b".repeat(40);
        let queued = format!("{:040x}", 0);
        let history = log(vec![
            json!({"author":"user","content":"start"}),
            json!({"author":"user","content":"also this"}),
        ]);
        let error = request_start_disposition(&history, &run, &queued).unwrap_err();
        assert!(error.contains("no admission event"), "{error}");
    }

    #[test]
    fn recorded_start_is_idempotent_and_conflicting_start_is_rejected() {
        let run = "b".repeat(40);
        let other = "c".repeat(40);
        let queued = format!("{:040x}", 0);
        let history = log(vec![
            json!({"author":"user","content":"start"}),
            json!({"request":run,"request_head":queued,"status":"queued"}),
            json!({"request":run,"status":"running"}),
        ]);
        assert_eq!(
            request_start_disposition(&history, &run, &queued).unwrap(),
            RequestStart::Running
        );
        let error = request_start_disposition(&history, &other, &queued).unwrap_err();
        assert!(error.contains(&run), "{error}");
    }

    #[test]
    fn durably_admitted_request_transitions_from_queued_to_running() {
        let run = "b".repeat(40);
        let queued = format!("{:040x}", 0);
        let history = log(vec![
            json!({"author":"user","content":"start"}),
            json!({"request":run,"request_head":queued,"status":"queued"}),
            json!({"author":"user","content":"also this"}),
        ]);
        assert_eq!(
            request_start_disposition(&history, &run, &queued).unwrap(),
            RequestStart::Claim {
                expected: "f".repeat(40),
                tree: "a".repeat(40),
            }
        );
    }

    #[test]
    fn request_admission_rejects_a_mismatched_anchor() {
        let run = "b".repeat(40);
        let queued = format!("{:040x}", 0);
        let history = log(vec![
            json!({"author":"user","content":"start"}),
            json!({"request":run,"request_head":"c".repeat(40),"status":"queued"}),
        ]);
        let error = request_start_disposition(&history, &run, &queued).unwrap_err();
        assert!(error.contains("records head"), "{error}");
    }

    #[test]
    fn terminal_old_request_cannot_reclaim_after_new_request_is_queued() {
        let old = "b".repeat(40);
        let new = "c".repeat(40);
        let old_head = format!("{:040x}", 0);
        let new_head = format!("{:040x}", 4);
        let history = log(vec![
            json!({"author":"user","content":"old"}),
            json!({"request":old,"request_head":old_head,"status":"queued"}),
            json!({"request":old,"status":"running"}),
            json!({"request":old,"status":"failed"}),
            json!({"author":"user","content":"new"}),
            json!({"request":new,"request_head":new_head,"status":"queued"}),
        ]);
        assert_eq!(
            request_start_disposition(&history, &old, &old_head).unwrap(),
            RequestStart::Running
        );
        assert_eq!(
            request_start_disposition(&history, &new, &new_head).unwrap(),
            RequestStart::Claim {
                expected: "f".repeat(40),
                tree: "a".repeat(40),
            }
        );
    }

    #[test]
    fn pending_append_rebuilds_after_a_head_race() {
        let attempts = std::cell::RefCell::new(Vec::new());
        let mut reloads = 0;
        let committed = retry_pending_append(
            "head-0",
            "workspace-0",
            |head, workspace| {
                attempts
                    .borrow_mut()
                    .push((head.to_string(), workspace.to_string()));
                if head == "head-0" {
                    Ok(progress::ConditionalAppend::HeadChanged(
                        "head-1".to_string(),
                    ))
                } else {
                    Ok(progress::ConditionalAppend::Appended(
                        progress::AppendResult {
                            commit: "pending-commit".to_string(),
                            previous_head: head.to_string(),
                            retries: 0,
                        },
                    ))
                }
            },
            || {
                reloads += 1;
                Ok(ReloadedPending {
                    head: "head-1".to_string(),
                    workspace: Some("workspace-1-with-pending".to_string()),
                })
            },
        )
        .unwrap();
        assert_eq!(committed, "pending-commit");
        assert_eq!(reloads, 1);
        assert_eq!(
            attempts.into_inner(),
            [
                ("head-0".to_string(), "workspace-0".to_string()),
                ("head-1".to_string(), "workspace-1-with-pending".to_string())
            ]
        );
    }

    #[test]
    fn pending_append_accepts_a_concurrent_durable_record() {
        let mut attempts = 0;
        let head = retry_pending_append(
            "head-0",
            "workspace-0",
            |_, _| {
                attempts += 1;
                Ok(progress::ConditionalAppend::HeadChanged(
                    "head-1".to_string(),
                ))
            },
            || {
                Ok(ReloadedPending {
                    head: "head-1".to_string(),
                    workspace: None,
                })
            },
        )
        .unwrap();
        assert_eq!(head, "head-1");
        assert_eq!(attempts, 1);
    }

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
