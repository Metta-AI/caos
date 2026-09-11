//! Recording a Claude Code session as an ordinary CAOS conversation.
//!
//! Claude Code drives the model; CAOS keeps the durable log. Every hook Claude
//! Code fires arrives here as one JSON object on stdin, so the whole surface is
//! a single `caos cc hook` command and `.claude/settings.json` needs no shell
//! at all — no `jq`, no quoting, none of the constructs CLAUDE.md catalogs as
//! this tree's most reliable source of bugs. The payload names its own event
//! (`hook_event_name`), so one command serves every hook.
//!
//! The conversation these events build is an ordinary one: the same head ref,
//! the same append-only spine, the same transitions `caos tui` already replays
//! (design/chat.md). Its id is derived from the Claude Code session id rather
//! than stored in a side table, so there is no local state to lose or corrupt:
//! the ref is the whole record.
//!
//! AND THE TOOLS ARE ORDINARY TOOLS. Nothing here implements one. A call is
//! recorded as the model's own declaration and then handed to `llm-step` in its
//! tools-only mode, which runs it exactly as it runs a call in a turn it drives
//! itself — so `caos cc` offers whatever that step offers, in whatever
//! repository it is pointed at, and a tool reads one way wherever a model meets
//! it. The step is named by `--llm-step`, as `tui` and `chat` name it.
//!
//! What Claude Code keeps is the MODEL. It chose the call and will read the
//! result; the request a prompt admits is claimed here rather than by a worker,
//! because the turn is already running by the time the hook fires.

mod serve;

use std::io::Read;

use serde_json::{json, Value};

use std::collections::BTreeMap;

use caos::{GitTransport, Transport};
use conversation_protocol::v3::apply::{apply, inherited_signature, mint, Transition};
use conversation_protocol::v3::canonical::canonical_bytes;
use conversation_protocol::v3::git_store::GitStore;
use conversation_protocol::v3::oid::{Oid, G3};
use conversation_protocol::v3::paths;
use conversation_protocol::v3::records::{
    Block, DeclaredCall, Identity, IdentityKind, RequestOutcome, RequestRecord, RequestStatus,
    Role, TranscriptEntry,
};
use conversation_protocol::v3::refs;
use conversation_protocol::v3::tree::Signature;
use conversation_protocol::v3::view::Conversation;
use conversation_protocol::v3::{CodeOps, ObjectStore};

use crate::{
    conversation_ref, default_title, default_workspace_name, ensure_code_commit,
    fetch_validated_head, mint_transition, oid, open_store, push_cas, reject_reserved_caos,
    resolve_base, resolve_username, signature, update_local_cache, TurnOptions, LLM_STEP_ARG,
    MAX_APPEND_ATTEMPTS,
};

/// Conversation ids for recorded sessions live under one component so they are
/// obvious in the sidebar and cannot collide with a hand-named conversation.
/// `ConversationId::parse` accepts it: Claude Code session ids are lowercase
/// hex and dashes.
const SESSION_PREFIX: &str = "cc/";

/// Claude Code names an MCP tool `mcp__<server>__<tool>`. Only calls to our own
/// server get a session injected; everything else this hook sees is left
/// exactly as the model wrote it.
const TOOL_PREFIX: &str = "mcp__caos__";

/// Where a workspace keeps the tools it defines itself. The step reads this
/// name out of the tree it is handed (`std/llm-step`'s `tree_tools_dir`); the
/// client reads it off disk, to decide whether handing over a tree is worth
/// what it costs.
const TREE_TOOLS_DIR: &str = "caos-tools";

/// What a request record says ran the turn. Claude Code chooses the model and
/// does not tell a hook which, so naming the harness is the honest answer --
/// better than a plausible-looking model string nothing verified.
const CLAUDE_CODE_MODEL: &str = "claude-code";

/// The workspace arrives UNRESOLVED because only `serve` can carry on without
/// one. A hook that cannot find the repository has nothing to record into and
/// should say so; a tool server that cannot find it still has to answer, or
/// the session sees `CONNECTION_CLOSED` and no reason at all.
///
/// `--llm-step:<type>=<value>` names the step that runs the tools, exactly as
/// `tui` and `chat` take it: a session in another repository points it at
/// wherever that repository mounted caos.
pub fn cli_cc(workspace: Result<GitTransport, String>, args: &[String]) -> Result<(), String> {
    let mut options = TurnOptions::default();
    let mut rest = Vec::new();
    for argument in args {
        if !options.take_image_arg(argument) {
            rest.push(argument.as_str());
        }
    }
    match rest.first().copied() {
        Some("hook") => hook(&workspace?, &options),
        Some("serve") => serve::serve(workspace, options),
        _ => Err(usage()),
    }
}

fn usage() -> String {
    format!(
        "usage:\n  \
         caos cc hook    (reads one Claude Code hook payload on stdin)\n  \
         caos cc serve   (workspace tool server; JSON-RPC on stdio)\n\n\
         Both name the step whose tools this session offers:\n  \
         {}",
        crate::missing_image_arg(LLM_STEP_ARG)
    )
}

/// What one tool call answers with: the observation the transcript kept, and
/// whether the tool itself failed. A failed tool is a RESULT, not an error --
/// the model is meant to read it and react.
struct ToolOutcome {
    text: String,
    is_error: bool,
}

/// Run one workspace tool, by declaring the call and handing it to `llm-step`.
///
/// TWO STEPS, and the second is the whole design: cc records the call as the
/// model's own declaration -- v3 accepts no tool that no message declared
/// (`validate_current_call`) -- and then runs the step in its tools-only mode,
/// which starts the tool, executes it and completes it exactly as it does for a
/// turn it drives itself. Nothing here knows what `edit` is. That is why a
/// session in another repository gets the same tools as the tui: they are the
/// step's, reached through `--llm-step`, not a copy of them compiled in here.
fn run_tool(
    _serve_transport: &GitTransport,
    options: &TurnOptions,
    session: &str,
    name: &str,
    args: &Value,
) -> Result<ToolOutcome, String> {
    // A FRESH transport, not the one `serve` opened at spawn. That one was read
    // once, and in a cloud session it can predate the SessionStart hook that
    // adds the `caos` remote -- so a call driven through it dials a server it
    // does not know and fails "no `caos` git remote" while the remote is right
    // there. The resolver thread re-opens per attempt for exactly this reason
    // (see `resolve_in_background`); a call has to as well.
    let fresh = GitTransport::from_cwd()
        .map_err(|error| format!("cannot open the caos workspace for this call: {error}"))?;
    let t = &fresh;
    let id = conversation_id_for(session)?;
    // The prompt hook that opens this conversation runs in another process and
    // takes seconds; the first tool call can beat it. Wait for the record
    // rather than refuse the call. A no-op on every turn but the racing first.
    wait_for_conversation(t, &id);
    let call = args
        .get("caos_tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or(name)
        .to_string();
    let declared = declared_args(args);
    let username = resolve_username(t, None)?;

    // The request this call belongs to, captured from the attempt that won the
    // compare-and-swap. Its round is the one BEFORE the declaration: a
    // `model.complete` opens the next round, and the call belongs to the round
    // that declared it.
    let mut declaration: Option<(Oid, Oid, u64)> = None;
    append(t, &id, |store, head| {
        let view = Conversation::open(store, head)?;
        let Some(request) = view.active_request()? else {
            return Err(format!(
                "no active request in {id:?}: a tool call arrived before this session's prompt"
            ));
        };
        let ordinal = view.transcript_len()?;
        drop(view);

        let round = request.round;
        let signature = inherited_signature(store, head)?;
        let message_id = caos::fresh_entropy()?;
        let dir = paths::transcript_payload_dir(ordinal, &message_id);
        let admitted = paths::admit_external_id(&call);
        let payload_name = format!("args-{admitted}.json");
        let entry = TranscriptEntry {
            message_id,
            conversation: id.to_string(),
            role: Role::Assistant,
            actor: username.clone(),
            request: Some(request.id.clone()),
            round: Some(round),
            model: Some(CLAUDE_CODE_MODEL.to_string()),
            blocks: vec![Block::ToolUse {
                id: call.clone(),
                name: name.to_string(),
                arguments: format!("{dir}/{payload_name}"),
            }],
            proposal: None,
            workspace_resolution: None,
        };
        declaration = Some((request.id.clone(), request.request_head.clone(), round));
        Ok(Some(mint_transition(
            store,
            head,
            &Transition::ModelComplete {
                request: request.id.clone(),
                entry,
                payloads: vec![(payload_name, payload_bytes(&declared)?)],
                calls: vec![DeclaredCall {
                    id: call.clone(),
                    name: name.to_string(),
                }],
            },
            &signature,
        )?))
    })?;
    let (request, request_head, round) =
        declaration.ok_or_else(|| "the call was never declared".to_string())?;

    dispatch_call(t, options, &id, &request, &request_head, &call)?;
    read_outcome(t, &id, &request, round, &call)
}

/// Run the declared call, as `llm-step` in its tools-only mode.
///
/// The request is the one the prompt admitted, named by `--run` rather than
/// implied: this ArgTree is not that one, and must not be, or a second call
/// would be answered from the first's memo. `--tools-only` carries the call id
/// for exactly that reason, and the step checks it ran before returning.
fn dispatch_call(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    request: &Oid,
    request_head: &Oid,
    call: &str,
) -> Result<(), String> {
    let store = caos::build_secret_store(t)?;
    let configuration = tools_configuration(t, options, id, &store)?;
    let dispatch = caos::prepare_client_request_with_store(
        t,
        &configuration,
        &[
            format!("--head:commit={request_head}"),
            format!("--run={request}"),
            format!("--tools-only={call}"),
        ],
        &store,
    )?;
    let server = t.server_url()?;
    caos::compute_client_request_with_store(&server, &dispatch, &store).map(drop)
}

/// The step a call runs on: `llm-step`, curried with everything that is the
/// same for every call of this conversation.
///
/// It is the request's recorded `configuration` too, so a conversation says
/// which worker ran its tools -- and the tui can pick the turn up, because what
/// it names is an ordinary step.
/// Resolve the `--llm-step` image, ONCE PER CONTAINER, shared across this
/// session's processes.
///
/// `serve`'s resolver, the prompt hook and every tool dispatch all resolve the
/// same step, in SEPARATE processes whose in-memory eval memo (`caos-eval`)
/// cannot be shared. Each resolve is ~9s of round trips, and at session start
/// they run CONCURRENTLY over one tunnel and starve each other -- which wedges
/// the hook recording the first prompt and fails the whole first turn (later
/// turns work because by then the record exists). Measured, that resolve is the
/// dominant cost of the ~14s prompt hook.
///
/// So a file under the temp dir holds the resolved oid, and a `create_new`
/// marker single-flights it: the first process to arrive resolves and writes
/// the oid; the others find the marker, poll the (LOCAL) file until it appears,
/// and reuse it -- no second resolve, no second tunnel user, no wedge. Keyed by
/// the step arg AND the secret store, because the resolution depends on both
/// (design/secrets.md) and the client-side resolve marks the step's tools with
/// the caller's identity. The oid is deterministic in those inputs, so a cached
/// entry can only be absent, never wrong.
///
/// Every filesystem failure falls back to a direct resolve: caching must never
/// be what stops a session from working.
fn resolve_step_cached(
    t: &GitTransport,
    options: &TurnOptions,
    store: &[caos::ClientSecret],
) -> Result<String, String> {
    let direct = || crate::resolve_image_arg(t, options.llm_step.as_deref(), LLM_STEP_ARG, store);
    let Some(arg) = options.llm_step.as_deref() else {
        return direct();
    };
    // FNV-1a over (arg, secret scope): a stable cross-process key with no new
    // dependency (a hashing crate would have to be anchored in the cargo bake).
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in arg
        .as_bytes()
        .iter()
        .chain(b"\0")
        .chain(caos::secret_store_header(store).as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // A FIXED shared directory, not `temp_dir()`: the whole point is that
    // `serve`, the hook and each tool dispatch find the SAME file, and Claude
    // Code can spawn those processes with different `TMPDIR`s -- measured, the
    // hook resolved anew (29s) beside a resolver that had already cached, so
    // they were not sharing. `/tmp` is one path all of them agree on in the
    // container; only where it is missing does this fall back to `temp_dir()`.
    let dir = if std::path::Path::new("/tmp").is_dir() {
        std::path::PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let cache = dir.join(format!("caos-cc-step-{hash:016x}"));
    let marker = dir.join(format!("caos-cc-step-{hash:016x}.flight"));

    let read_cache = || -> Option<String> {
        let oid = std::fs::read_to_string(&cache).ok()?;
        let oid = oid.trim();
        (!oid.is_empty()).then(|| oid.to_string())
    };

    // ~5 min of patience for a cold resolve, polling the local file: far longer
    // than a resolve takes, so a live winner is always waited out; long enough
    // that a dead one is reclaimed below rather than hung on forever.
    for _ in 0..300 {
        if let Some(oid) = read_cache() {
            return Ok(oid);
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            // We own the flight: resolve, publish atomically, release.
            Ok(_) => {
                let result = direct();
                if let Ok(oid) = &result {
                    let tmp = dir.join(format!("caos-cc-step-{hash:016x}.{}", std::process::id()));
                    if std::fs::write(&tmp, oid).is_ok() {
                        let _ = std::fs::rename(&tmp, &cache);
                    }
                }
                let _ = std::fs::remove_file(&marker);
                return result;
            }
            // Someone else owns it. Reclaim a marker whose owner died (older
            // than any real resolve), else wait for the cache to appear.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&marker)
                    .and_then(|m| m.modified())
                    .and_then(|t| t.elapsed().map_err(std::io::Error::other))
                    .map(|age| age.as_secs() > 300)
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(&marker);
                    continue;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            // The lock dir is unusable; do not let that stop the session.
            Err(_) => return direct(),
        }
    }
    // Waited out the budget without a result -- resolve directly rather than
    // hang the caller.
    direct()
}

fn tools_configuration(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    store: &[caos::ClientSecret],
) -> Result<String, String> {
    // REACHABLE FIRST, IN FRONT OF EVERYTHING BELOW. Resolving the step and
    // preparing a request both talk to the caos server, and in a cloud session
    // that server is reached through a tunnel. A tunnel whose far end is gone
    // accepts and swallows, so those waits are unbounded -- and this is the
    // path a HOOK takes. A `UserPromptSubmit` hook that never returns takes the
    // session's prompt with it, which presents as Claude Code hanging on the
    // first thing you type, with nothing anywhere saying why.
    //
    // But a single 5s probe LOSES A RACE it must not lose. The SessionStart
    // hook brings the tunnel up, and the first prompt can fire before it is
    // proven -- and then this one probe fails, the whole conversation is never
    // created, and every tool call of that first turn is refused with "no
    // conversation to record into" while the tools list perfectly well (the
    // serve resolver retries; this did not). So it retries, still bounded:
    // enough to outlast tunnel bringup, never enough to hang the prompt.
    wait_server_reachable(t)?;
    let mut config = vec![format!("--conversation={id}")];
    let merge_refs = crate::snapshot_merge_refs(t)?;
    if !merge_refs.is_empty() {
        config.push(format!("--merge-refs={merge_refs}"));
    }
    let base = resolve_step_cached(t, options, store)?;
    crate::curry_client_object(t, &base, &config).map(|hash| hash.to_string())
}

/// Wait, bounded, for the caos server to answer -- the hook's counterpart to
/// the serve resolver's retry loop.
///
/// The budget is a compromise between the two ways this hurts. Too short and
/// the first prompt races tunnel bringup and loses (the bug this exists for);
/// too long and a genuinely-down server hangs the prompt, since a
/// `UserPromptSubmit` hook blocks the turn until it returns. Each attempt is
/// `ensure_server_reachable`'s own 5s round trip, so `HOOK_REACH_ATTEMPTS`
/// probes plus the interstitial sleeps is the ceiling -- ~35s, which outlasts
/// tunnel bringup and stays well under Claude Code's hook timeout. The common
/// case returns on the first probe.
const HOOK_REACH_ATTEMPTS: u32 = 6;
const HOOK_REACH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

fn wait_server_reachable(t: &GitTransport) -> Result<(), String> {
    let mut last = t.ensure_server_reachable();
    let mut attempt = 1;
    while last.is_err() && attempt < HOOK_REACH_ATTEMPTS {
        std::thread::sleep(HOOK_REACH_INTERVAL);
        last = t.ensure_server_reachable();
        attempt += 1;
    }
    last
}

/// How long a tool call waits for its conversation to exist before proceeding
/// to the append that would refuse it.
///
/// The conversation is created by the `UserPromptSubmit` hook -- a SEPARATE
/// process, whose work measures ~12s in a cloud session: it is the CURRY AND
/// PUSH of the request over the tunnel, not the fetch (a local `--llm-step`
/// path is exactly as slow), and it does not cache, so every prompt pays it.
/// Claude Code does not hold the turn for that, so the model's first tool call
/// arrives before it lands, and refusing it ("no conversation to record into")
/// is what makes a session's WHOLE FIRST TURN fail while every later turn works.
///
/// So a call WAITS for the record the prompt hook is pushing -- but SPARSELY,
/// which is the whole subtlety. The probe is an ls-remote to the caos server,
/// and the prompt hook is pushing to that same server through the same
/// single-stream tunnel; a tight poll competes with the push for it and can
/// starve the very thing it waits for -- measured, a 1s poll wedged the push so
/// it never completed and the wait timed out against a conversation that would
/// otherwise have landed at ~12s. Spaced probes leave the tunnel to the push.
/// `fetch_validated_head` returns early on an absent ref, without a fetch or a
/// local write, so a probe is cheap; the interval is what matters. Bounded well
/// past the ~12s; a conversation that never appears still errors, just later.
const TOOL_WAIT_ATTEMPTS: u32 = 15;
const TOOL_WAIT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(4);

fn wait_for_conversation(t: &GitTransport, id: &str) {
    for attempt in 0..TOOL_WAIT_ATTEMPTS {
        if let Ok(store) = open_store(t) {
            if matches!(fetch_validated_head(t, &store, id), Ok(Some(_))) {
                return;
            }
        }
        if attempt + 1 < TOOL_WAIT_ATTEMPTS {
            std::thread::sleep(TOOL_WAIT_INTERVAL);
        }
    }
}

/// The observation the step recorded, read back from the conversation.
///
/// Read rather than returned: the run's own result names the conversation
/// commit, but what Claude Code needs is one call's text, and the transcript is
/// where that lives. A tool the model can act on and a tool the tui replays are
/// then the same bytes.
fn read_outcome(
    t: &GitTransport,
    id: &str,
    request: &Oid,
    round: u64,
    call: &str,
) -> Result<ToolOutcome, String> {
    let store = open_store(t)?;
    let (_, head) = fetch_validated_head(t, &store, id)?
        .ok_or_else(|| format!("conversation {id:?} disappeared while its tool ran"))?;
    let view = Conversation::open(&store, &head)?;
    let tool = view
        .tool(request, round, call)?
        .ok_or_else(|| format!("the step recorded no call {call} in round {round}"))?;
    let (is_error, text) = crate::protocol_tool_result(&view, tool.result.as_ref())?;
    Ok(ToolOutcome { text, is_error })
}

/// The tools this session offers, as MCP declarations.
///
/// Asked of the step that implements them, so there is one description of
/// `edit` wherever a model meets it, and a tool a repository defines under
/// `caos-tools/` is offered here exactly as it is in the tui.
fn declarations(t: &GitTransport, options: &TurnOptions) -> Result<Vec<Value>, String> {
    let store = caos::build_secret_store(t)?;
    let base = resolve_step_cached(t, options, &store)?;
    let mut kvs = vec!["--list-tools=1".to_string()];
    // The tree whose `caos-tools/` entries are offered -- named ONLY when
    // there are any, and the ordering is the whole point.
    //
    // Naming it PUSHES THE WHOLE WORKING TREE to the caos server, which for a
    // repository of any size is the most expensive thing this client does, and
    // for a repository with no `caos-tools/` it buys an answer of "none". So
    // the cheap local question is asked first: a directory that is not there
    // defines no tools.
    //
    // Worse than wasteful without it. A caos server that REFUSES is harmless
    // -- the error is caught and the listing goes on without the tree -- but
    // one reached through a tunnel whose far end is gone does not refuse. It
    // accepts and swallows, so the push never returns, the resolution never
    // finishes, and the session gets a tool server that is connected and
    // permanently empty.
    if t.work_dir().join(TREE_TOOLS_DIR).is_dir() {
        if let Ok(workspace) = resolve_base(t, options) {
            let mut objects = open_store(t)?;
            let workspace = oid(&workspace, "conversation base")?;
            ensure_code_commit(t, &mut objects, &workspace)?;
            let tree = objects.tree_of(&workspace)?;
            kvs.push(format!("--workspace:hash={tree}"));
        }
    }
    let (_, result) = caos::run_client_request_with_store(t, &base, &kvs, &store)?;
    let objects = open_store(t)?;
    let result = oid(&result, "tool registry")?;
    objects.ensure_local(&result)?;
    let bytes = objects.read_blob(&result).map_err(String::from)?;
    serde_json::from_slice(&bytes).map_err(|error| format!("parsing the tool registry: {error}"))
}

/// The model's own arguments, without the values the hook injected: those are
/// plumbing, and showing them in the transcript would misrepresent the call the
/// model actually made.
fn declared_args(args: &Value) -> Value {
    let mut declared = args.clone();
    if let Some(object) = declared.as_object_mut() {
        object.remove("caos_session");
        object.remove("caos_tool_use_id");
        object.remove("caos_prompt_id");
    }
    declared
}

/// Dispatch one hook payload. An event we do not record is not an error: Claude
/// Code fires many, and a settings file that routes extra ones here should keep
/// working rather than failing a turn.
fn hook(t: &GitTransport, options: &TurnOptions) -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| format!("reading hook payload: {error}"))?;
    let payload: Value =
        serde_json::from_str(&input).map_err(|error| format!("parsing hook payload: {error}"))?;
    let event = string_field(&payload, "hook_event_name")?;
    // Logged at the START too, so a hook killed mid-run (its process gone before
    // the end line) is distinguishable from one that never fired.
    debug_log_hook("start", event, &payload, None);
    let started = std::time::Instant::now();
    let result = match event {
        "UserPromptSubmit" => on_user_prompt(t, options, &payload),
        "PreToolUse" => on_pre_tool_use(&payload),
        "Stop" => on_stop(t, &payload),
        "StopFailure" => on_stop_failure(t, &payload),
        _ => Ok(()),
    };
    debug_log_hook("end", event, &payload, Some((started.elapsed(), &result)));
    result
}

/// A best-effort line per hook invocation, appended to `$CAOS_CC_HOOK_LOG`, or
/// to `<tmp>/caos-cc-hook.log` when that is unset. The hooks are the one part
/// of this that runs in a process nobody watches -- Claude Code spawns them and
/// keeps only a pass/fail -- so when the conversation a session should have is
/// simply absent, there is otherwise nothing to say which hook fired, for which
/// session, and whether it returned or failed and why.
fn debug_log_hook(
    phase: &str,
    event: &str,
    payload: &Value,
    done: Option<(std::time::Duration, &Result<(), String>)>,
) {
    let path = match std::env::var("CAOS_CC_HOOK_LOG") {
        Ok(path) if !path.is_empty() => std::path::PathBuf::from(path),
        _ => std::env::temp_dir().join("caos-cc-hook.log"),
    };
    let session = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let prompt_len = payload
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::len)
        .unwrap_or(0);
    let tail = match done {
        None => String::new(),
        Some((elapsed, Ok(()))) => format!(" {:.1}s ok", elapsed.as_secs_f64()),
        Some((elapsed, Err(error))) => {
            format!(" {:.1}s ERR {}", elapsed.as_secs_f64(), error.replace('\n', " "))
        }
    };
    let line = format!("{phase} {event} session={session} prompt_len={prompt_len}{tail}\n");
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// The user's prompt, and the only event allowed to create the conversation:
/// the first prompt of a session establishes its base and fallback title
/// exactly as the TUI's first message does.
fn on_user_prompt(t: &GitTransport, options: &TurnOptions, payload: &Value) -> Result<(), String> {
    let id = conversation_id(payload)?;
    let prompt = string_field(payload, "prompt")?;
    if prompt.trim().is_empty() {
        return Ok(());
    }
    record_prompt(t, options, &id, prompt)
}

/// Record a user's prompt, creating the conversation on the session's first one.
///
/// THREE TRANSITIONS, minted in one compare-and-swap: the message, the request
/// it opens, and the claim that puts the request in `running`. v3 requires the
/// last of those before any tool may be recorded (`require_request_running_or_-
/// cancelling`), and Claude Code has already begun the turn by the time this
/// hook fires -- so admitting and claiming together is not a shortcut, it is
/// the truth: no runner is going to pick this request up.
///
/// The request is a REAL ONE all the same -- an ArgTree over the step, prepared
/// exactly as the tui prepares a turn -- because each of the turn's tool calls
/// runs it. A request id that named nothing would leave the calls nothing to
/// run and the record claiming a configuration that never existed.
/// Print `cc-timing: <phase> <secs>` to stderr when `CAOS_CC_TIMING` is set.
/// A `record_prompt` measures ~12s in a cloud session and it is not obvious
/// which of its four server round trips owns that; this makes each one report.
/// Where the hook debug log lives: `$CAOS_CC_HOOK_LOG`, else `<tmp>/caos-cc-hook.log`.
fn hook_log_path() -> std::path::PathBuf {
    match std::env::var("CAOS_CC_HOOK_LOG") {
        Ok(path) if !path.is_empty() => std::path::PathBuf::from(path),
        _ => std::env::temp_dir().join("caos-cc-hook.log"),
    }
}

/// Append one best-effort line to the hook debug log.
fn append_hook_log(line: &str) {
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(hook_log_path())
    {
        let _ = file.write_all(line.as_bytes());
    }
}

fn cc_timing(phase: &str, elapsed: std::time::Duration) {
    append_hook_log(&format!("  timing {phase} {:.2}s\n", elapsed.as_secs_f64()));
}

fn record_prompt(
    t: &GitTransport,
    options: &TurnOptions,
    id: &str,
    prompt: &str,
) -> Result<(), String> {
    let username = resolve_username(t, None)?;
    let signature = signature(&username)?;
    let refname = conversation_ref(id)?;
    let secrets = caos::build_secret_store(t)?;
    let phase = std::time::Instant::now();
    let configuration = tools_configuration(t, options, id, &secrets)?;
    cc_timing("tools_configuration", phase.elapsed());

    for _ in 0..MAX_APPEND_ATTEMPTS {
        let mut store = open_store(t)?;
        let observed = fetch_validated_head(t, &store, id)?.map(|(_, head)| head);
        let head = match &observed {
            Some(head) => head.clone(),
            None => root_commit(t, &mut store, id, prompt, options, &signature)?,
        };

        // The message. Its id is fresh entropy, as the client's own is: a
        // transcript entry is addressed by it, and two prompts in one session
        // must not collide.
        let message_id = caos::fresh_entropy()?;
        let entry = TranscriptEntry {
            message_id: message_id.clone(),
            conversation: id.to_string(),
            role: Role::User,
            actor: username.clone(),
            request: None,
            round: None,
            model: None,
            blocks: vec![Block::Text {
                text: prompt.to_string(),
            }],
            proposal: None,
            workspace_resolution: None,
        };
        let message = mint_transition(
            &mut store,
            &head,
            &Transition::MessageAppend {
                entry,
                payloads: Vec::new(),
            },
            &signature,
        )?;

        // The request, prepared exactly as a turn the tui dispatches is: an
        // ArgTree over the step, pinned to the head the prompt left. A tool
        // call runs THIS request rather than one of its own, so what the step
        // is handed -- the conversation, the head it was admitted at -- is what
        // the record says it was.
        let phase = std::time::Instant::now();
        let request = oid(
            &caos::prepare_client_request_with_store(
                t,
                &configuration,
                &[format!("--head:commit={message}")],
                &secrets,
            )?,
            "request",
        )?;
        cc_timing("prepare_request", phase.elapsed());
        let view = Conversation::open(&store, &message)?;
        let workspaces = view.workspaces_tree()?;
        drop(view);
        let record = RequestRecord {
            id: request.clone(),
            request_head: message.clone(),
            request_workspaces: workspaces,
            model: CLAUDE_CODE_MODEL.to_string(),
            configuration: configuration.clone(),
            round: 0,
            calls: Vec::new(),
            interjections: Vec::new(),
            status: RequestStatus::Queued,
            latest_message: None,
            escape_reason: None,
            outcome: None,
        };
        let admission = inherited_signature(&store, &message)?;
        let admitted = mint_transition(
            &mut store,
            &message,
            &Transition::RequestAdmit { record },
            &admission,
        )?;
        let claimed = mint_transition(
            &mut store,
            &admitted,
            &Transition::RequestClaim {
                request,
                latest_message: message_id,
            },
            &admission,
        )?;

        let phase = std::time::Instant::now();
        let pushed = push_cas(&store, &refname, observed.as_ref(), &claimed)?;
        cc_timing("push_cas", phase.elapsed());
        if pushed {
            let _ = update_local_cache(t, &refname, claimed.as_str());
            return Ok(());
        }
    }
    Err(format!(
        "conversation {id:?} head moved during all {MAX_APPEND_ATTEMPTS} append attempts"
    ))
}

/// The conversation's root commit, for a session whose first prompt this is.
fn root_commit(
    t: &GitTransport,
    store: &mut GitStore,
    id: &str,
    prompt: &str,
    options: &TurnOptions,
    signature: &Signature,
) -> Result<Oid, String> {
    let phase = std::time::Instant::now();
    let base = oid(&resolve_base(t, options)?, "conversation base")?;
    cc_timing("resolve_base", phase.elapsed());
    let phase = std::time::Instant::now();
    ensure_code_commit(t, store, &base)?;
    cc_timing("ensure_code_commit", phase.elapsed());
    reject_reserved_caos(t, base.as_str(), "base workspace")?;
    let workspace = default_workspace_name(t, options)?;
    let genesis = oid(G3, "v3 genesis")?;
    let root = Transition::ConversationRoot {
        identity: Identity {
            id: id.to_string(),
            kind: IdentityKind::Root,
            owner: None,
        },
        title: default_title(prompt),
        workspaces: BTreeMap::from([(workspace, (base, None))]),
        files_seed: None,
    };
    let tree = apply(store, None, &root)?.tree;
    mint(store, &genesis, &tree, root.kind(), signature)
}

/// Tell a caos workspace tool which conversation it is working in.
///
/// The tool server is spawned once per session and is otherwise stateless, so
/// this is the only thing that attributes a call to a conversation. Both values
/// are declared in every tool's schema rather than smuggled in, so the model's
/// own call stays schema-valid and this hook only fills in values the tool
/// already accepted.
///
/// No `permissionDecision` is emitted. Injection and permission are separate
/// concerns: allowing these tools without a prompt is a choice for
/// `permissions.allow`, not something a hook should decide on the user's behalf
/// just because it happened to be in the call path.
fn on_pre_tool_use(payload: &Value) -> Result<(), String> {
    let tool = string_field(payload, "tool_name")?;
    if !tool.starts_with(TOOL_PREFIX) {
        return Ok(());
    }
    let session = string_field(payload, "session_id")?;
    let mut input = payload
        .get("tool_input")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let Some(object) = input.as_object_mut() else {
        return Err(format!("{tool} was called with a non-object tool_input"));
    };
    object.insert("caos_session".to_string(), json!(session));
    if let Some(id) = payload.get("tool_use_id").and_then(Value::as_str) {
        object.insert("caos_tool_use_id".to_string(), json!(id));
    }
    // The turn this call belongs to, which becomes the event's `request` — the
    // same job `llm-step`'s `run` does for a turn it drives itself.
    if let Some(prompt) = payload.get("prompt_id").and_then(Value::as_str) {
        object.insert("caos_prompt_id".to_string(), json!(prompt));
    }
    let response = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": input,
        }
    });
    println!(
        "{}",
        serde_json::to_string(&response)
            .map_err(|error| format!("encoding hook response: {error}"))?
    );
    Ok(())
}

/// A session's conversation id is derived, never stored: the ref is the only
/// state, so there is no map to fall out of step with the sessions it names.
fn conversation_id(payload: &Value) -> Result<String, String> {
    conversation_id_for(string_field(payload, "session_id")?)
}

/// A session id is the one ref component we do not author, so it is validated
/// through the protocol's own parser rather than trusted — whether it arrives
/// in a hook payload or as a tool argument.
fn conversation_id_for(session: &str) -> Result<String, String> {
    let id = format!("{SESSION_PREFIX}{session}");
    refs::validate_conversation_id(&id)?;
    Ok(id)
}

fn string_field<'a>(payload: &'a Value, key: &str) -> Result<&'a str, String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("hook payload has no string {key}"))
}

/// Append whatever `step` mints, compare-and-swapping the conversation head.
///
/// `step` returns the new head, or `None` when there is nothing to record --
/// a hook that fires for a turn this session never opened, say. The retries are
/// not a concurrency model: the tool server handles one call at a time. They
/// protect against ANOTHER writer, such as an interjection typed into the tui.
fn append(
    t: &GitTransport,
    id: &str,
    mut step: impl FnMut(&mut GitStore, &Oid) -> Result<Option<Oid>, String>,
) -> Result<(), String> {
    let refname = conversation_ref(id)?;
    for _ in 0..MAX_APPEND_ATTEMPTS {
        let mut store = open_store(t)?;
        let Some((_, head)) = fetch_validated_head(t, &store, id)? else {
            return Err(format!(
                "no conversation {id:?} to record into; \
                 a session's first recorded event is its user prompt"
            ));
        };
        let Some(candidate) = step(&mut store, &head)? else {
            return Ok(());
        };
        if push_cas(&store, &refname, Some(&head), &candidate)? {
            let _ = update_local_cache(t, &refname, candidate.as_str());
            return Ok(());
        }
    }
    Err(format!(
        "conversation {id:?} head moved during all {MAX_APPEND_ATTEMPTS} append attempts"
    ))
}

/// A payload's bytes, in the protocol's canonical framing.
fn payload_bytes(value: &Value) -> Result<Vec<u8>, String> {
    const PREFIX: &[u8] = b"{\"payload\":";
    let wrapped = canonical_bytes(&json!({ "payload": value }))?;
    if !wrapped.starts_with(PREFIX) || !wrapped.ends_with(b"}\n") {
        return Err("canonical payload wrapper had an unexpected shape".to_string());
    }
    Ok(wrapped[PREFIX.len()..wrapped.len() - 2].to_vec())
}

/// The assistant's closing message, and the end of the request it answered.
fn on_stop(t: &GitTransport, payload: &Value) -> Result<(), String> {
    let id = conversation_id(payload)?;
    let message = payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let username = resolve_username(t, None)?;
    let conversation = id.clone();
    append(t, &id, move |store, head| {
        let _ = &conversation;
        let view = Conversation::open(store, head)?;
        let Some(request) = view.active_request()? else {
            // Nothing to close. A Stop for a turn that opened no request is
            // not an error: an interrupted session can leave one behind.
            return Ok(None);
        };
        let ordinal = view.transcript_len()?;
        let round = request.round;
        drop(view);

        let signature = inherited_signature(store, head)?;
        let mut at = head.clone();
        let mut result = None;
        if !message.trim().is_empty() {
            let message_id = caos::fresh_entropy()?;
            let entry = TranscriptEntry {
                message_id: message_id.clone(),
                conversation: conversation.clone(),
                role: Role::Assistant,
                actor: username.clone(),
                request: Some(request.id.clone()),
                round: Some(round),
                model: Some(CLAUDE_CODE_MODEL.to_string()),
                blocks: vec![Block::Text {
                    text: message.clone(),
                }],
                proposal: None,
                workspace_resolution: None,
            };
            at = mint_transition(
                store,
                &at,
                &Transition::ModelComplete {
                    request: request.id.clone(),
                    entry,
                    payloads: Vec::new(),
                    calls: Vec::new(),
                },
                &signature,
            )?;
            // The turn's result is that message, by path -- what a reader
            // shows for the turn, and the same thing `llm-step` names.
            result = Some(paths::transcript_entry_path(ordinal, &message_id));
        }
        let terminal = mint_transition(
            store,
            &at,
            &Transition::RequestTerminal {
                request: request.id.clone(),
                outcome: RequestOutcome::Idle {
                    result,
                    interrupted: false,
                },
            },
            &signature,
        )?;
        Ok(Some(terminal))
    })
}

/// A turn that ended badly closes its request as failed.
fn on_stop_failure(t: &GitTransport, payload: &Value) -> Result<(), String> {
    let id = conversation_id(payload)?;
    let kind = payload
        .get("error_type")
        .and_then(Value::as_str)
        .unwrap_or("error");
    let detail = payload
        .get("error_message")
        .and_then(Value::as_str)
        .unwrap_or("");
    let error = match detail.trim().is_empty() {
        true => kind.to_string(),
        false => format!("{kind}: {detail}"),
    };
    let username = resolve_username(t, None)?;
    let conversation = id.clone();
    append(t, &id, move |store, head| {
        let view = Conversation::open(store, head)?;
        let Some(request) = view.active_request()? else {
            return Ok(None);
        };
        let ordinal = view.transcript_len()?;
        let round = request.round;
        drop(view);

        // The outcome names a TRANSCRIPT ENTRY, not the text: `apply` validates
        // it as a path, and the tui reads the failure by following it. So the
        // message is recorded first and the terminal points at it.
        let signature = inherited_signature(store, head)?;
        let message_id = caos::fresh_entropy()?;
        let recorded = mint_transition(
            store,
            head,
            &Transition::MessageAppend {
                entry: TranscriptEntry {
                    message_id: message_id.clone(),
                    conversation: conversation.clone(),
                    role: Role::System,
                    actor: username.clone(),
                    request: Some(request.id.clone()),
                    round: Some(round),
                    model: None,
                    blocks: vec![Block::Text {
                        text: error.clone(),
                    }],
                    proposal: None,
                    workspace_resolution: None,
                },
                payloads: Vec::new(),
            },
            &signature,
        )?;
        let terminal = mint_transition(
            store,
            &recorded,
            &Transition::RequestTerminal {
                request: request.id,
                outcome: RequestOutcome::Failed {
                    error: paths::transcript_entry_path(ordinal, &message_id),
                },
            },
            &signature,
        )?;
        Ok(Some(terminal))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_become_valid_conversation_ids() {
        let payload = json!({"session_id": "88888abc-9ae5-4d07-a44d-54b366776bdc"});
        assert_eq!(
            conversation_id(&payload).unwrap(),
            "cc/88888abc-9ae5-4d07-a44d-54b366776bdc"
        );
    }

    /// A session id is the one part of a conversation id we do not author.
    /// v3 puts the id in its ref path HEX-ENCODED, so a hostile one cannot
    /// escape the namespace or name a directory -- which is what this pins,
    /// rather than a rejection the protocol deliberately does not perform.
    #[test]
    fn hostile_session_ids_cannot_escape_the_ref_namespace() {
        for hostile in ["../../etc", "a/../b", "with space", "head", "a.lock"] {
            let payload = json!({ "session_id": hostile });
            let id = conversation_id(&payload).unwrap();
            let refname = conversation_ref(&id).unwrap();
            assert!(
                refname.starts_with("refs/caos/v3/conversations/"),
                "session id {hostile:?} named {refname}"
            );
            assert_eq!(refs::parse_head_ref(&refname).unwrap(), id);
        }
    }

    #[test]
    fn a_payload_without_its_event_name_is_an_error() {
        assert!(string_field(&json!({}), "hook_event_name").is_err());
        assert!(string_field(&json!({"hook_event_name": 2}), "hook_event_name").is_err());
    }
}
