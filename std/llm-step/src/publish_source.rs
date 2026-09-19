//! Pin publication intent in the conversation before any remote mutation.
use super::*;
use conversation_protocol::v3::publication::Outcome;
use conversation_protocol::v3::{Descriptor, PublicationRecord, PublicationStatus};

pub(super) const HELP: &str = "Publish the exact selected source commit to an HTTPS Git repository branch, preserving its history. Test and inspect the intended PR diff first. Resolve merge conflicts and clear .caos/conflicts before publishing. The endpoint rejects files matched by the source commit's .gitignore rules, including tracked files. Remove those files or adjust the rules before publishing. It never strips files or rewrites commits. This does not create a PR or change the source gitlink. Updates default to fast-forward. Set rewrite=true only to publish intentionally rebased history; the exact remote-head lease still prevents overwriting a concurrent change. A receipt names the exact published commit even if the source later changes. On uncertainty, inspect the remote before taking another action.
@param repository HTTPS Git repository URL, without credentials.
@param branch Destination branch name (without refs/heads/).
@param [rewrite] Allow an intentional history rewrite while retaining the exact remote-head lease.";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    source_tree: String,
    repository: String,
    branch: String,
    #[serde(default)]
    rewrite: bool,
}

fn parameters(call: &Call) -> Result<Parameters, String> {
    let p: Parameters = serde_json::from_value(call.input.clone())
        .map_err(|e| format!("invalid publish_source arguments: {e}"))?;
    paths::validate_source_tree_name(&p.source_tree)?;
    git_locator::import::remote(&p.repository)?;
    conversation_protocol::v3::source_trees::validate_branch(&p.branch)?;
    if p.branch.starts_with("refs/") {
        return Err("branch must omit refs/heads/".into());
    }
    Ok(p)
}

fn pin_path(site: &CallSite<'_>) -> String {
    format!(
        "{}/publication.json",
        paths::call_payload_dir(site.request.as_str(), site.round, &site.call.id)
    )
}

fn pinned(
    view: &Conversation<'_>,
    site: &CallSite<'_>,
) -> Result<Option<PublicationRecord>, String> {
    if view
        .tool(site.request, site.round, &site.call.id)?
        .is_none()
    {
        return Ok(None);
    }
    let id: String = serde_json::from_slice(&view.payload(&pin_path(site))?)
        .map_err(|_| "invalid publication pin")?;
    view.publication(&id)?
        .ok_or("pinned publication is missing".into())
        .map(Some)
}

pub(super) fn execute(state: &mut progress::State, site: &CallSite<'_>) -> Result<(), String> {
    let p = match parameters(site.call) {
        Ok(p) => p,
        Err(error) => return site.fail(state, &error),
    };
    let token_file = import_source::token_file(&p.repository);
    let token = token_file
        .map(fs::read_to_string)
        .transpose()
        .map_err(|_| "reading GitHub token")?;
    let token = token.as_deref().map(str::trim_end);
    let observe = || {
        git_locator::publish::read_branch(&p.repository, &p.branch, token)
            .and_then(|h| h.map(|h| Oid::parse(&h, "remote head")).transpose())
    };
    let pending = match pinned(&state.conversation()?, site)? {
        Some(record) => record,
        None => {
            let view = state.conversation()?;
            let head = match view.source_tree(&p.source_tree)? {
                Some(source) => source.commit,
                None => {
                    return site.fail(state, "publish_source requires an existing source gitlink")
                }
            };
            let base = view.reference_start(&p.source_tree)?;
            let old = match observe() {
                Ok(old) => old,
                Err(error) => return site.fail(state, &error),
            };
            let id = view.identity()?.id;
            let descriptor = Descriptor {
                source_base: base.clone(),
                source_head: head.clone(),
                target_base: base,
                policy: if p.rewrite { "rewrite" } else { "preserve" }.into(),
                implementation: "caos/server-push".into(),
                commit_policy: "preserve".into(),
            };
            let key = invocation(&id, site)?[..32].to_string();
            let publication = ids::publication_id(
                &id,
                &key,
                &ids::projection_id(&descriptor.to_value())?,
                &head,
                &p.repository,
                &format!("refs/heads/{}", p.branch),
                old.as_ref(),
            )?;
            let record = PublicationRecord {
                id: publication,
                key,
                descriptor,
                planned_head: head,
                repository: p.repository.clone(),
                refname: format!("refs/heads/{}", p.branch),
                expected_old: old,
                source_tree_name: p.source_tree.clone(),
                status: PublicationStatus::Pending,
                evidence: None,
                observed: None,
            };
            let Some(record) = pin(state, site, &record)? else {
                return Ok(());
            };
            record
        }
    };
    if pending.status != PublicationStatus::Pending {
        return finish(state, site, &pending, None);
    }
    // An attempt that pinned this intent may send it, including concurrent
    // attempts that joined the identical transition. The exact lease makes
    // those pushes converge. Recovery never obtains a fresh source or lease.
    let outcome = {
        let mut command = std::process::Command::new("caos");
        command
            .args([
                "push-git",
                &pending.repository,
                pending.planned_head.as_str(),
                &p.branch,
            ])
            .arg(format!(
                "--expected={}",
                pending
                    .expected_old
                    .as_ref()
                    .map(Oid::as_str)
                    .unwrap_or("absent")
            ));
        if pending.descriptor.policy == "rewrite" {
            command.arg("--rewrite");
        }
        if let Some(file) = token_file {
            command.arg(format!("--github-token-file={file}"));
        }
        let child = command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        match child {
            Err(_) => Outcome::new(
                PublicationStatus::Conflict,
                "validation-rejected",
                Some("Could not start caos push-git; no push was attempted.".into()),
                None,
            ),
            Ok(child) => match child.wait_with_output() {
                Ok(output) if output.status.success() => {
                    let value: Value = serde_json::from_slice(&output.stdout)
                        .map_err(|_| "invalid push-git result")?;
                    let status: PublicationStatus = serde_json::from_value(value["status"].clone())
                        .map_err(|_| "invalid push status")?;
                    let observed: Option<Oid> = serde_json::from_value(value["observed"].clone())
                        .map_err(|_| "invalid remote head")?;
                    let outcome = Outcome::new(
                        status,
                        value["kind"]
                            .as_str()
                            .ok_or("missing publication evidence")?,
                        value["diagnostic"].as_str().map(str::to_owned),
                        observed,
                    );
                    reconcile(&pending, outcome, observe)
                }
                Ok(output) if output.status.code() == Some(1) => Outcome::new(
                    PublicationStatus::Conflict,
                    "validation-rejected",
                    Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
                    None,
                ),
                _ => recovered(&pending, observe()),
            },
        }
    };
    finish(state, site, &pending, Some(outcome))
}

pub(super) fn invocation(conversation: &str, site: &CallSite<'_>) -> Result<String, String> {
    ids::protocol_id(
        "external-tool",
        &json!({
            "conversation":conversation, "request":site.request, "round":site.round, "call":site.call.id
        }),
    )
}

pub(super) fn reconcile(
    pending: &PublicationRecord,
    outcome: Outcome,
    observe: impl FnOnce() -> Result<Option<Oid>, String>,
) -> Outcome {
    // A local/server validation refusal means no push was attempted. A remote
    // head that already matches must not hide the rejection or its diagnostic.
    if outcome.status == PublicationStatus::Complete
        || outcome.evidence.kind == "validation-rejected"
    {
        return outcome;
    }
    match observe() {
        Ok(head) if head.as_ref() == Some(&pending.planned_head) => {
            Outcome::new(PublicationStatus::Complete, "ref-converged", None, head)
        }
        observed if outcome.status == PublicationStatus::Uncertain => recovered(pending, observed),
        _ => outcome,
    }
}

fn recovered(pending: &PublicationRecord, observed: Result<Option<Oid>, String>) -> Outcome {
    match observed {
        Ok(head) => Outcome::from_observation(
            pending,
            head,
            "No confirmed push result. An unchanged branch may still have an in-flight update."
                .into(),
            false,
        ),
        Err(_) => Outcome::new(
            PublicationStatus::Uncertain,
            "ambiguous",
            Some(
                "Could not observe the pinned publication; inspect the remote before continuing."
                    .into(),
            ),
            None,
        ),
    }
}

pub(super) fn pin<S: progress::RefStore>(
    state: &mut progress::State<S>,
    site: &CallSite<'_>,
    pending: &PublicationRecord,
) -> Result<Option<PublicationRecord>, String> {
    for _ in 0..32 {
        state.reload()?;
        let view = state.conversation()?;
        if view
            .tool(site.request, site.round, &site.call.id)?
            .is_some_and(|r| r.is_terminal())
        {
            return Ok(None);
        }
        if let Some(record) = pinned(&view, site)? {
            return Ok(Some(record));
        }
        let mut tool = site.stub(None);
        tool.status = CallStatus::Started;
        let expected = state.head().clone();
        if matches!(
            state.try_append_pair_at(
                &expected,
                Transition::PublicationPending {
                    record: pending.clone()
                },
                Transition::ToolStart {
                    record: tool,
                    payloads: vec![(
                        "publication.json".into(),
                        canonical_payload_bytes(&json!(pending.id))?
                    )]
                }
            )?,
            progress::TryAppend::Appended(_)
        ) {
            return Ok(Some(pending.clone()));
        }
    }
    Err("conversation kept moving while pinning publication".into())
}

pub(super) fn finish<S: progress::RefStore>(
    state: &mut progress::State<S>,
    site: &CallSite<'_>,
    pending: &PublicationRecord,
    outcome: Option<Outcome>,
) -> Result<(), String> {
    for _ in 0..32 {
        state.reload()?;
        let view = state.conversation()?;
        if view
            .tool(site.request, site.round, &site.call.id)?
            .is_some_and(|r| r.is_terminal())
        {
            return Ok(());
        }
        let mut record = view
            .publication(&pending.id)?
            .ok_or("publication disappeared")?;
        let mut transitions = Vec::new();
        if record.status == PublicationStatus::Pending {
            let out = outcome.as_ref().ok_or("missing publication outcome")?;
            record.status = out.status;
            record.evidence = Some(out.evidence.clone());
            record.observed = out.observed.clone();
            transitions.push(Transition::PublicationTerminal {
                publication: record.id.clone(),
                status: out.status,
                evidence: out.evidence.clone(),
                observed: out.observed.clone(),
            });
        }
        let text = serde_json::to_string(&record).map_err(|e| e.to_string())?;
        let block = result_block(
            &site.call.id,
            &text,
            record.status != PublicationStatus::Complete,
        );
        let stub = site.stub(None);
        let tool = completed_record(
            &stub,
            ToolResult::Complete {
                observation: observation_path(&stub),
                proposal: None,
            },
            None,
        );
        transitions.push(tool_complete_transition(tool, &block, Vec::new())?);
        let expected = state.head().clone();
        if matches!(
            state.try_append_many_at(&expected, transitions)?,
            progress::TryAppend::Appended(_)
        ) {
            return Ok(());
        }
    }
    Err("conversation kept moving while completing publication".into())
}
