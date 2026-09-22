//! Push a pinned stack through the same branch publication path as publish_source.
use super::*;
use conversation_protocol::v3::publication::Outcome;
use conversation_protocol::v3::{GitStore, PublicationRecord, PublicationStatus};

pub(super) fn declaration() -> Value {
    json!({
        "name":"push_stack",
        "description":"Push a registered stack's branches directly from the CAOS server to an HTTPS Git repository. Branch names are the source gitlink paths. Pins all commits and remote-head leases before pushing bottom to top. Does not create PRs or GitHub stack membership, and needs no source checkout or GitHub worker. Finish or abort pending conflicts and restack edited lower layers first. Set rewrite=true for intentionally rebased history; exact leases still reject concurrent branch changes. Pushes are not atomic across branches: inspect the returned receipts after partial failure. Recovery keeps the original commits and leases and skips recorded successful pushes.",
        "input_schema":{"type":"object","additionalProperties":false,"properties":{
            "path":{"type":"string","description":"Registered stack directory."},
            "repository":{"type":"string","description":"HTTPS Git repository URL, without credentials."},
            "rewrite":{"type":"boolean","description":"Allow intentionally rewritten history with an exact remote-head lease."}
        },"required":["path","repository"]}
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    path: String,
    repository: String,
    #[serde(default)]
    rewrite: bool,
}

fn payload(site: &CallSite<'_>) -> String {
    format!(
        "{}/publications.json",
        paths::call_payload_dir(site.request.as_str(), site.round, &site.call.id)
    )
}

fn saved(
    view: &Conversation<'_>,
    site: &CallSite<'_>,
) -> Result<Option<Vec<PublicationRecord>>, String> {
    if view
        .tool(site.request, site.round, &site.call.id)?
        .is_none()
    {
        return Ok(None);
    }
    serde_json::from_slice(&view.payload(&payload(site))?)
        .map(Some)
        .map_err(|_| "invalid pinned stack publication".into())
}

fn plan(state: &progress::State, site: &CallSite<'_>) -> Result<Vec<PublicationRecord>, String> {
    let p: Parameters = serde_json::from_value(site.call.input.clone())
        .map_err(|e| format!("invalid push_stack arguments: {e}"))?;
    git_locator::import::remote(&p.repository)?;
    let server = std::env::var("CAOS_SERVER_URL").map_err(|_| "CAOS_SERVER_URL not set")?;
    let store = GitStore::scratch_partial(&fresh_name("push-stack-objects"), &server)?;
    let view = state.conversation()?;
    let branches = stack::push_sources(&view, &p.path, &store)?;
    let token = import_source::token_file(&p.repository)
        .map(fs::read_to_string)
        .transpose()
        .map_err(|_| "reading GitHub token")?;
    branches
        .into_iter()
        .map(|branch| {
            let old = git_locator::publish::read_branch(
                &p.repository,
                &branch,
                token.as_deref().map(str::trim_end),
            )?
            .map(|h| Oid::parse(&h, "remote head"))
            .transpose()?;
            publish_source::plan(&view, site, &branch, &p.repository, &branch, old, p.rewrite)
        })
        .collect()
}

pub(super) fn execute(state: &mut progress::State, site: &CallSite<'_>) -> Result<(), String> {
    if state
        .conversation()?
        .tool(site.request, site.round, &site.call.id)?
        .is_some_and(|r| r.is_terminal())
    {
        return Ok(());
    }
    let plans = match saved(&state.conversation()?, site)? {
        Some(plans) => plans,
        None => match plan(state, site) {
            Ok(plans) => plans,
            Err(error) => return site.fail(state, &error),
        },
    };
    let Some(plans) = pin(state, site, plans)? else {
        return Ok(());
    };
    advance(state, site, &plans, publish_source::push)
}

fn pin<S: progress::RefStore>(
    state: &mut progress::State<S>,
    site: &CallSite<'_>,
    plans: Vec<PublicationRecord>,
) -> Result<Option<Vec<PublicationRecord>>, String> {
    for _ in 0..32 {
        state.reload()?;
        let view = state.conversation()?;
        if view
            .tool(site.request, site.round, &site.call.id)?
            .is_some_and(|r| r.is_terminal())
        {
            return Ok(None);
        }
        if let Some(plans) = saved(&view, site)? {
            return Ok(Some(plans));
        }
        let mut record = site.stub(None);
        record.status = CallStatus::Started;
        let expected = state.head().clone();
        if matches!(
            state.try_append_at(
                &expected,
                Transition::ToolStart {
                    record,
                    payloads: vec![(
                        "publications.json".into(),
                        canonical_payload_bytes(&json!(plans))?
                    )],
                }
            )?,
            progress::TryAppend::Appended(_)
        ) {
            return Ok(Some(plans));
        }
    }
    Err("conversation kept moving while pinning stack publication".into())
}

fn advance<S: progress::RefStore>(
    state: &mut progress::State<S>,
    site: &CallSite<'_>,
    plans: &[PublicationRecord],
    mut push: impl FnMut(&PublicationRecord) -> Result<Outcome, String>,
) -> Result<(), String> {
    let mut receipts = Vec::new();
    for plan in plans {
        state.reload()?;
        let record = match state.conversation()?.publication(&plan.id)? {
            Some(record) => record,
            None => {
                state.append(Transition::PublicationPending {
                    record: plan.clone(),
                })?;
                plan.clone()
            }
        };
        let record = if record.status == PublicationStatus::Pending {
            let outcome = push(&record)?;
            publish_source::retain(state, &record, outcome)?
        } else {
            record
        };
        let complete = record.status == PublicationStatus::Complete;
        receipts.push(record);
        if !complete {
            break;
        }
    }
    let complete = receipts.len() == plans.len()
        && receipts
            .iter()
            .all(|r| r.status == PublicationStatus::Complete);
    let remaining: Vec<_> = plans[receipts.len()..].iter().map(|r| &r.refname).collect();
    let result = json!({"status":if complete {"complete"} else {"partial"}, "branches":receipts, "not_attempted":remaining});
    let block = result_block(&site.call.id, &result.to_string(), !complete);
    let stub = site.stub(None);
    let record = completed_record(
        &stub,
        ToolResult::Complete {
            observation: observation_path(&stub),
            proposal: None,
        },
        None,
    );
    let transition = tool_complete_transition(record, &block, Vec::new())?;
    for _ in 0..32 {
        state.reload()?;
        if state
            .conversation()?
            .tool(site.request, site.round, &site.call.id)?
            .is_some_and(|r| r.is_terminal())
        {
            return Ok(());
        }
        let head = state.head().clone();
        if matches!(
            state.try_append_at(&head, transition.clone())?,
            progress::TryAppend::Appended(_)
        ) {
            return Ok(());
        }
    }
    Err("conversation kept moving while completing stack publication".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{golden_with_first, ImportStore};

    fn fixture() -> (progress::State<ImportStore>, Oid, Call, String) {
        let args = json!({"path":"feature", "repository":"https://example.com/repo.git"});
        let golden = golden_with_first("push_stack", args.clone()).unwrap();
        let request = golden.request.clone();
        let declaration = Conversation::open(&golden.store, &golden.head)
            .unwrap()
            .transcript_entry(1)
            .unwrap()
            .unwrap()
            .1
            .message_id;
        let call = Call {
            id: "first".into(),
            name: "push_stack".into(),
            input: args,
        };
        let store = ImportStore {
            objects: golden.store,
            head: golden.head.clone(),
            race: None,
            lost_ack: true,
        };
        let mut state = progress::State::from_store(
            store,
            "refs/conversations/conversation/head".into(),
            golden.head,
        )
        .unwrap();
        for (name, c) in [("a", 'a'), ("b", 'b'), ("c", 'c')] {
            state
                .append(Transition::reference(
                    format!("feature/{name}"),
                    Some(Oid::parse(&c.to_string().repeat(40), "head").unwrap()),
                ))
                .unwrap();
        }
        (state, request, call, declaration)
    }

    fn plans(state: &progress::State<ImportStore>, site: &CallSite<'_>) -> Vec<PublicationRecord> {
        ["a", "b", "c"]
            .into_iter()
            .map(|name| {
                let path = format!("feature/{name}");
                publish_source::plan(
                    &state.conversation().unwrap(),
                    site,
                    &path,
                    "https://example.com/repo.git",
                    &path,
                    Some(Oid::parse(&"d".repeat(40), "old").unwrap()),
                    true,
                )
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn restart_reuses_pinned_stack_and_skips_completed_branches() {
        let (mut state, request, call, declaration) = fixture();
        let site = CallSite::at(&request, 0, &call, &declaration);
        let original = plans(&state, &site);
        let pinned = pin(&mut state, &site, original.clone()).unwrap().unwrap();
        let mut calls = 0;
        let failed = advance(&mut state, &site, &pinned, |record| {
            calls += 1;
            if calls == 2 {
                return Err("worker stopped before receiving a result".into());
            }
            Ok(Outcome::new(
                PublicationStatus::Complete,
                "push-success",
                None,
                Some(record.planned_head.clone()),
            ))
        });
        assert!(failed.is_err());
        state
            .append(Transition::reference(
                "feature/b".into(),
                Some(Oid::parse(&"e".repeat(40), "edit").unwrap()),
            ))
            .unwrap();
        let mut newer = plans(&state, &site);
        newer[1].expected_old = None;
        let recovered = pin(&mut state, &site, newer).unwrap().unwrap();
        assert_eq!(recovered, original);
        let mut pushed = Vec::new();
        advance(&mut state, &site, &recovered, |record| {
            pushed.push(record.refname.clone());
            assert_eq!(record.expected_old, original[1].expected_old);
            Ok(Outcome::new(
                PublicationStatus::Complete,
                "ref-converged",
                None,
                Some(record.planned_head.clone()),
            ))
        })
        .unwrap();
        assert_eq!(pushed, ["refs/heads/feature/b", "refs/heads/feature/c"]);
        assert!(pin(&mut state, &site, original).unwrap().is_none());
        validate_spine(state.store(), state.head(), &mut HashSet::new()).unwrap();
    }

    #[test]
    fn failed_layer_reports_partial_progress_and_does_not_push_upper_layers() {
        for status in [PublicationStatus::Conflict, PublicationStatus::Uncertain] {
            let (mut state, request, call, declaration) = fixture();
            let site = CallSite::at(&request, 0, &call, &declaration);
            let plans = plans(&state, &site);
            pin(&mut state, &site, plans.clone()).unwrap().unwrap();
            let mut calls = 0;
            advance(&mut state, &site, &plans, |record| {
                calls += 1;
                Ok(if calls == 1 {
                    Outcome::new(
                        PublicationStatus::Complete,
                        "push-success",
                        None,
                        Some(record.planned_head.clone()),
                    )
                } else {
                    Outcome::new(
                        status,
                        "lease-rejected",
                        Some("remote changed".into()),
                        None,
                    )
                })
            })
            .unwrap();
            assert_eq!(calls, 2);
            let view = state.conversation().unwrap();
            assert_eq!(
                view.publication(&plans[0].id).unwrap().unwrap().status,
                PublicationStatus::Complete
            );
            assert_eq!(
                view.publication(&plans[1].id).unwrap().unwrap().status,
                status
            );
            assert!(view.publication(&plans[2].id).unwrap().is_none());
            let record = view.tool(&request, 0, &call.id).unwrap().unwrap();
            let observation: Value =
                serde_json::from_slice(&view.payload(&observation_path(&record)).unwrap()).unwrap();
            assert_eq!(observation["is_error"], true);
            let receipt: Value =
                serde_json::from_str(observation["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(receipt["status"], "partial");
            assert_eq!(receipt["not_attempted"], json!(["refs/heads/feature/c"]));
            validate_spine(state.store(), state.head(), &mut HashSet::new()).unwrap();
        }
    }
}
