//! The GitHub worker is independent of source-tree materialization.
use super::*;

pub(super) fn declaration() -> Value {
    json!({
        "name":"github",
        "description":"Run GitHub CLI with literal arguments and an explicit owner/repository. Supports issues, comments, PRs and remote stack operations. Every new tool call observes GitHub afresh; retries never repeat an unfinished invocation. After an uncertain write, use a new read call to inspect the outcome; an absent result does not prove an in-flight write failed. For PRs, first publish_source, then find/create a PR with explicit --head and --base; preserve existing human-edited titles/bodies. Create ready PRs unless a draft was requested. For stacks, publish source gitlinks bottom to top, each including the exact published lower commit. Use gh stack link --base <mainline> with existing PR URLs in order; verify bases and membership afterwards. Link can extend a stack, not remove or reorder it. submit/sync/rebase require local branches and are not supported by this worker. Multi-step failures can leave partial changes.",
        "input_schema":{"type":"object","additionalProperties":false,"properties":{
            "repository":{"type":"string","description":"GitHub owner/repository."},
            "args":{"type":"array","items":{"type":"string"},"minItems":1,"description":"Arguments after gh, e.g. [\"pr\",\"list\",\"--head\",\"feature/a\",\"--json\",\"url,baseRefName,headRefOid\"]."},
            "stdin":{"type":"string","description":"Optional standard input; pass bodies with --body-file -."}
        },"required":["repository","args"]}
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    repository: String,
    args: Vec<String>,
    stdin: Option<String>,
}

pub(super) fn start(
    cfg: &Config,
    state: &mut progress::State,
    site: &CallSite<'_>,
) -> Result<bool, String> {
    if cfg.github_source.is_none() {
        site.fail(state, "GitHub worker is unavailable")?;
        return Ok(true);
    }
    // The enclosing model turn must also be isolated by the GitHub identity.
    // Its grant is already required for import_source and publish_source.
    if secret("github-token").is_err() {
        site.fail(
            state,
            "Grant github-token to both std/llm-step and std/github before using GitHub tools",
        )?;
        return Ok(true);
    }
    let me = self_curry(
        None,
        site.request,
        site.round,
        &site.call.id,
        &[
            ("current-tool", Arg::Lit("github")),
            ("tool-eval", Arg::Lit("github")),
        ],
    )?;
    // Keep source data in the agent image. Embedding a secret-marked worker
    // changes its bindings and prevents the agent's own reader grant matching.
    eval_then_catching(&arg("github-source"), "DEEP-DEPS/github", Arg::Hash(&me))?;
    Ok(false)
}

pub(super) fn evaluated(
    cfg: &Config,
    state: &mut progress::State,
    request: &Oid,
    request_head: &Oid,
    round: u64,
    id: &str,
) -> Result<(), String> {
    let view = state.conversation()?;
    let record = require_request(&view, request)?;
    let current = round_state(&view, &record)?;
    if record.status != TurnStatus::Running
        || current.declaring_round != round
        || view.tool(request, round, id)?.is_some()
    {
        return resume(cfg, state, request, request_head);
    }
    let call = current
        .pending
        .iter()
        .find(|call| call.id == id)
        .cloned()
        .ok_or("evaluated GitHub call is no longer pending")?;
    let site = CallSite::at(request, round, &call, &current.declaration_message);
    if Path::new(&arg("error")).exists() {
        site.fail(state, &read_arg("error")?)?;
        return resume(cfg, state, request, request_head);
    }
    let image = cas_hash(&arg("result"))?;
    if launch(&image, state, &site)? {
        resume(cfg, state, request, request_head)
    } else {
        Ok(())
    }
}

fn launch(image: &str, state: &mut progress::State, site: &CallSite<'_>) -> Result<bool, String> {
    let p: Parameters = match serde_json::from_value(site.call.input.clone()) {
        Ok(p) => p,
        Err(error) => {
            site.fail(state, &format!("invalid github arguments: {error}"))?;
            return Ok(true);
        }
    };
    let parts: Vec<_> = p.repository.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|p| {
            p.is_empty()
                || p.starts_with('.')
                || p.starts_with('-')
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
        || p.args.is_empty()
        || p.args.iter().any(|a| a.contains('\0'))
    {
        site.fail(
            state,
            "github requires owner/repository and a nonempty array of literal arguments",
        )?;
        return Ok(true);
    }
    let invocation = publish_source::invocation(&state.conversation()?.identity()?.id, site)?;
    let args = serde_json::to_string(&p.args).map_err(|e| e.to_string())?;
    let mut bindings = vec![
        ("repository", Arg::Lit(&p.repository)),
        ("args", Arg::Lit(&args)),
        ("invocation", Arg::Lit(&invocation)),
    ];
    if let Some(stdin) = p.stdin.as_deref() {
        bindings.push(("stdin", Arg::Lit(stdin)));
    }
    let task = Oid::parse(
        &caos_curry(Arg::Hash(image), &bindings)?,
        "GitHub invocation",
    )?;
    let mut record = site.stub(None);
    record.status = CallStatus::Started;
    record.task = Some(task);
    let head = state.head().clone();
    match state.try_append_at(
        &head,
        Transition::ToolStart {
            record: record.clone(),
            payloads: Vec::new(),
        },
    )? {
        progress::TryAppend::HeadChanged(_) => Ok(true),
        progress::TryAppend::Appended(_) => {
            dispatch(site.request, site.round, site.call, &record)?;
            Ok(false)
        }
    }
}

pub(super) fn dispatch(
    request: &Oid,
    round: u64,
    call: &Call,
    record: &CallRecord,
) -> Result<(), String> {
    let me = self_curry(
        None,
        request,
        round,
        &call.id,
        &[("current-tool", Arg::Lit("github"))],
    )?;
    // Let the server assemble the request and attach its secret grant.
    // The worker ignores this immutable definition passed as run-then's input.
    worker_common::run_then_catching(
        &arg("github-source"),
        Arg::Hash(
            record
                .task
                .as_ref()
                .ok_or("GitHub start has no task")?
                .as_str(),
        ),
        Arg::Hash(&me),
    )
}

pub(super) fn result(record: &CallRecord) -> Result<(Value, Option<Oid>), String> {
    let text = read_arg("result")?;
    let value: Value = serde_json::from_str(&text).map_err(|_| "invalid GitHub worker result")?;
    let error = value["status"] != "complete" || value["exit"] != 0;
    // The full blob remains addressable through the normal tool result.
    let text = if text.len() > 100_000 {
        let mut start = text.len() - 100_000;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        format!(
            "Output truncated; showing the last 100000 bytes:\n{}",
            &text[start..]
        )
    } else {
        text
    };
    Ok((result_block(&record.id, &text, error), None))
}
