//! Merge a stack's pull requests, bottom to top, each squashed with its
//! `.pr.md` as the commit message (design/stack-publish.md, "stack-merge").
//! Everything is read before anything is merged: each layer to merge must have
//! an open pull request whose head is the layer's commit, and CI on every one
//! of those heads must be passing unless `force`. Then each PR is merged with
//! GitHub's asynchronous merge, one at a time, the next once GitHub has pointed
//! it at the trunk. A merge that fails stops the loop; the layers below it are
//! merged, the ones above are not touched, and running again carries on.
use std::thread::sleep;
use std::time::{Duration, Instant};

use super::*;
use conversation_protocol::v3::source_trees::validate_branch;

use crate::ci_status::{self, Checks, Standing};
use crate::publish_source::bool_or_string;
use crate::stack_publish::{github_repo, infer_trunk, provenance, read_stack, Github, Layer};
use crate::stack_sync::merged_pr;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    stack: String,
    #[serde(default, rename = "up-to", alias = "up_to")]
    up_to: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    trunk: Option<String>,
    #[serde(default, rename = "branch-prefix", alias = "branch_prefix")]
    branch_prefix: Option<String>,
    #[serde(default, deserialize_with = "bool_or_string")]
    force: bool,
    #[serde(
        default,
        rename = "dry-run",
        alias = "dry_run",
        deserialize_with = "bool_or_string"
    )]
    dry_run: bool,
}

fn blank(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

fn parameters(call: &Call) -> Result<Parameters, String> {
    let mut p: Parameters = serde_json::from_value(call.input.clone())
        .map_err(|error| format!("invalid stack-merge arguments: {error}"))?;
    p.stack = p.stack.trim().trim_matches('/').to_string();
    if p.stack.is_empty() {
        return Err("stack must name a directory, such as sources/repo".into());
    }
    paths::validate_source_tree_name(&p.stack)?;
    p.up_to = blank(p.up_to).map(|name| name.trim().trim_matches('/').to_string());
    p.repository = blank(p.repository);
    if let Some(repository) = &p.repository {
        git_locator::import::remote(repository)?;
    }
    p.trunk = blank(p.trunk);
    if let Some(trunk) = &p.trunk {
        validate_branch(trunk)?;
    }
    p.branch_prefix = blank(p.branch_prefix);
    Ok(p)
}

/// The described layers to merge, bottom first, up to and including `up_to`:
/// a layer's name (`02-tests`), its number (`02`) or its name without the
/// number (`tests`). None selects every described layer.
pub(super) fn select<'a>(
    layers: &'a [Layer],
    up_to: Option<&str>,
) -> Result<Vec<&'a Layer>, String> {
    let top = match up_to {
        None => layers.len(),
        Some(up_to) => {
            let position = layers.iter().position(|layer| {
                layer.name == up_to || layer.short == up_to || layer.name.get(..2) == Some(up_to)
            });
            let Some(position) = position else {
                return Err(format!("up-to {up_to:?} names no layer of the stack"));
            };
            if layers[position].description.is_none() {
                return Err(format!(
                    "up-to {}: it has no .pr.md, so it has no pull request to merge",
                    layers[position].name
                ));
            }
            position + 1
        }
    };
    let selected: Vec<&Layer> = layers[..top]
        .iter()
        .filter(|layer| layer.description.is_some())
        .collect();
    if selected.is_empty() {
        return Err("no layer has a .pr.md description, so none has a pull request to merge".into());
    }
    Ok(selected)
}

/// The squash commit's title: the PR's title and number, as GitHub writes it.
pub(super) fn commit_title(title: &str, number: u64) -> String {
    format!("{} (#{number})", title.trim())
}

#[derive(Clone, Debug)]
struct Pr {
    number: u64,
    head: String,
    base: String,
    draft: bool,
    merged: bool,
    /// In a GitHub stack, which retargets it itself after the PR below merges.
    stacked: bool,
}

fn pr_from(value: &Value) -> Option<Pr> {
    Some(Pr {
        number: value["number"].as_u64()?,
        head: value["head"]["sha"].as_str()?.to_string(),
        base: value["base"]["ref"].as_str()?.to_string(),
        draft: value["draft"].as_bool().unwrap_or(false),
        merged: !value["merged_at"].is_null() || value["merged"].as_bool().unwrap_or(false),
        stacked: value["stack"].is_object(),
    })
}

fn open_pr(github: &Github, branch: &str) -> Result<Option<Pr>, String> {
    let found = github.expect(
        "GET",
        &github.repo_path(&format!("/pulls?head={}:{branch}&state=open", github.owner)),
        None,
        &[200],
    )?;
    Ok(found.as_array().and_then(|prs| prs.first()).and_then(pr_from))
}

fn get_pr(github: &Github, number: u64) -> Result<Pr, String> {
    let value = github.expect(
        "GET",
        &github.repo_path(&format!("/pulls/{number}")),
        None,
        &[200],
    )?;
    pr_from(&value).ok_or_else(|| format!("GitHub answered for PR #{number} with no pull request"))
}

/// One layer as it will be merged, after the reads.
struct Target<'a> {
    layer: &'a Layer,
    branch: String,
    title: String,
    body: String,
    /// None when the layer's pull request is already merged.
    pr: Option<(Pr, Checks)>,
    already: Option<u64>,
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}

/// Wait for GitHub to point the PR at the trunk, which it does itself for a PR
/// in a GitHub stack once the PR below merges, and do it ourselves for one that
/// is not. Returns the PR as last read, whose head is what gets merged.
fn await_base(github: &Github, number: u64, trunk: &str) -> Result<Pr, String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let pr = get_pr(github, number)?;
        if pr.merged || pr.base == trunk {
            return Ok(pr);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "PR #{number} still targets {} two minutes after the layer below merged: GitHub has not retargeted it at {trunk} yet. Run stack-merge again in a minute; the layers below are merged.",
                pr.base
            ));
        }
        if pr.stacked {
            sleep(Duration::from_secs(3));
            continue;
        }
        github.expect(
            "PATCH",
            &github.repo_path(&format!("/pulls/{number}")),
            Some(&json!({"base": trunk})),
            &[200],
        )?;
    }
}

/// Follow an asynchronous merge to its end: the merge commit, or why it failed.
fn await_merge(github: &Github, number: u64, started: Value) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut answer = started;
    loop {
        let status = answer["status"].as_str().unwrap_or("");
        let details = &answer["details"];
        match status {
            "merged" => {
                if let Some(sha) = details["sha"].as_str() {
                    return Ok(sha.to_string());
                }
                let pr = github.expect(
                    "GET",
                    &github.repo_path(&format!("/pulls/{number}")),
                    None,
                    &[200],
                )?;
                return Ok(pr["merge_commit_sha"].as_str().unwrap_or("merged").to_string());
            }
            "failed" => {
                return Err(format!(
                    "GitHub could not merge PR #{number}: {}",
                    details["message"].as_str().unwrap_or("no reason given")
                ))
            }
            "enqueued" => {
                return Err(format!(
                    "PR #{number} was added to the merge queue, which merges it on its own schedule with the repository's default commit message. Run stack-merge again once it has merged, for the layers above."
                ))
            }
            _ => {}
        }
        let Some(uuid) = details["uuid"].as_str() else {
            return Err(format!(
                "GitHub accepted the merge of PR #{number} but gave no id to poll; check the PR before trying again"
            ));
        };
        if Instant::now() >= deadline {
            return Err(format!(
                "the merge of PR #{number} is still pending after five minutes (merge {uuid}); check the PR before trying again"
            ));
        }
        sleep(Duration::from_secs(3));
        answer = github.expect(
            "GET",
            &github.repo_path(&format!("/pulls/{number}/merge-async/{uuid}")),
            None,
            &[200],
        )?;
    }
}

pub(super) fn execute(state: &mut progress::State, site: &CallSite<'_>) -> Result<(), String> {
    let p = match parameters(site.call) {
        Ok(p) => p,
        Err(error) => return site.fail(state, &error),
    };
    let dir = p.stack.clone();
    let refused = |error: String| format!("stack-merge refused; nothing was merged: {error}");
    // Everything that reads the conversation, before anything reaches the remote.
    let read = {
        let view = state.conversation()?;
        read_stack(&view, &dir).and_then(|stack| {
            let prov = provenance(&view, &dir)?;
            let repository = p
                .repository
                .clone()
                .or_else(|| prov.repository.clone())
                .ok_or_else(|| {
                    format!("no repository: {dir}/00-base.source.json names none, so pass repository")
                })?;
            git_locator::import::remote(&repository)?;
            let trunk = p
                .trunk
                .clone()
                .or_else(|| infer_trunk(&prov))
                .ok_or_else(|| {
                    format!("no trunk: {dir}/00-base.source.json names no branch, so pass trunk")
                })?;
            validate_branch(&trunk)?;
            Ok((stack, repository, trunk))
        })
    };
    let (stack, repository, trunk) = match read {
        Ok(read) => read,
        Err(error) => return site.fail(state, &refused(error)),
    };
    let selected = match select(&stack.layers, p.up_to.as_deref()) {
        Ok(selected) => selected,
        Err(error) => return site.fail(state, &refused(error)),
    };
    let Some((owner, repo)) = github_repo(&repository) else {
        return site.fail(
            state,
            &refused(format!(
                "{repository} is not on github.com, and merging pull requests needs GitHub"
            )),
        );
    };
    let github = Github {
        owner,
        repo,
        token: ci_status::token_for(&repository)?,
    };
    let prefix = p.branch_prefix.clone().unwrap_or_default();

    // The reads: each layer's pull request and the checks on its head.
    let mut targets: Vec<Target<'_>> = Vec::new();
    for layer in selected {
        let branch = format!("{prefix}{}", layer.short);
        if let Err(error) = validate_branch(&branch) {
            return site.fail(state, &refused(format!("layer {}: {error}", layer.name)));
        }
        let description = layer.description.as_ref().expect("selected layers are described");
        let mut target = Target {
            layer,
            branch: branch.clone(),
            title: description.title.clone(),
            body: description.body.clone(),
            pr: None,
            already: None,
        };
        let pr = match open_pr(&github, &branch) {
            Ok(pr) => pr,
            Err(error) => return site.fail(state, &refused(error)),
        };
        match pr {
            None => match merged_pr(&github, &branch) {
                Ok(Some(number)) => target.already = Some(number),
                Ok(None) => {
                    return site.fail(
                        state,
                        &refused(format!(
                            "{dir}/{} has no open pull request on {branch}: run caos-std/stack-publish first",
                            layer.name
                        )),
                    )
                }
                Err(error) => return site.fail(state, &refused(error)),
            },
            Some(pr) => {
                if pr.draft {
                    return site.fail(
                        state,
                        &refused(format!(
                            "PR #{} ({}) is a draft: mark it ready for review first",
                            pr.number, layer.name
                        )),
                    );
                }
                if pr.head != layer.commit.as_str() {
                    return site.fail(
                        state,
                        &refused(format!(
                            "PR #{} holds {} but {dir}/{} is {}: run caos-std/stack-sync (to adopt what GitHub holds) and caos-std/stack-publish (to push the layer) so the pull request merges what the stack holds",
                            pr.number,
                            short(&pr.head),
                            layer.name,
                            short(layer.commit.as_str())
                        )),
                    );
                }
                let checks = match ci_status::read(&github, &pr.head) {
                    Ok(checks) => checks,
                    Err(error) => return site.fail(state, &refused(error)),
                };
                target.pr = Some((pr, checks));
            }
        }
        targets.push(target);
    }

    let mut lines: Vec<String> = Vec::new();
    let mut blockers: Vec<String> = Vec::new();
    for target in &targets {
        let mut line = format!("{}  {}", target.layer.name, target.branch);
        match (&target.pr, target.already) {
            (Some((pr, checks)), _) => {
                line.push_str(&format!("  PR #{}  checks: {}", pr.number, checks.summary()));
                if matches!(checks.verdict(), Some(Standing::Failing | Standing::Pending)) {
                    blockers.push(format!(
                        "{} (PR #{}): {}",
                        target.layer.name,
                        pr.number,
                        checks.summary()
                    ));
                }
            }
            (None, Some(number)) => line.push_str(&format!("  PR #{number}  already merged")),
            (None, None) => {}
        }
        lines.push(line);
    }
    let header = format!("repository {repository}  trunk {trunk}\n");
    if !blockers.is_empty() && !p.force {
        let mut text = header.clone();
        for line in &lines {
            text.push_str(line);
            text.push('\n');
        }
        text.push_str(&format!(
            "FAILED: stack-merge refused; nothing was merged: CI is not passing on {}. Wait for it or fix it (caos-std/ci-status reads one branch), or pass force=true to merge anyway.\n",
            blockers.join("; ")
        ));
        return site.complete(state, result_block(&site.call.id, &text, true), None, None, None);
    }
    if p.dry_run {
        let mut text = format!("dry run: nothing merged.\n{header}");
        for (target, line) in targets.iter().zip(&lines) {
            text.push_str(line);
            if let Some((pr, _)) = &target.pr {
                text.push_str(&format!(
                    "  would merge as {:?}",
                    commit_title(&target.title, pr.number)
                ));
            }
            text.push('\n');
        }
        if !blockers.is_empty() {
            text.push_str(&format!(
                "force=true: CI is not passing on {}.\n",
                blockers.join("; ")
            ));
        }
        return site.complete(state, result_block(&site.call.id, &text, false), None, None, None);
    }

    // The merges, bottom to top. Each layer's line gets its outcome.
    let mut problem: Option<String> = None;
    let mut merged = 0;
    for (target, line) in targets.iter().zip(lines.iter_mut()) {
        let Some((pr, _)) = &target.pr else {
            continue;
        };
        let number = pr.number;
        let current = match await_base(&github, number, &trunk) {
            Ok(current) => current,
            Err(error) => {
                problem = Some(error);
                line.push_str("  not merged");
                break;
            }
        };
        if current.merged {
            line.push_str("  already merged");
            continue;
        }
        let title = commit_title(&target.title, number);
        let mut body = json!({
            "sha": current.head,
            "merge_action": "direct_merge",
            "merge_method": "squash",
            "commit_title": title,
        });
        if !target.body.is_empty() {
            body["commit_message"] = json!(target.body);
        }
        let outcome = github
            .expect(
                "PUT",
                &github.repo_path(&format!("/pulls/{number}/merge-async")),
                Some(&body),
                &[200, 202, 409],
            )
            .and_then(|started| await_merge(&github, number, started));
        match outcome {
            Ok(sha) => {
                line.push_str(&format!("  merged {}  {title:?}", short(&sha)));
                merged += 1;
            }
            Err(error) => {
                problem = Some(error);
                line.push_str("  NOT MERGED");
                break;
            }
        }
    }

    let mut text = header;
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    if let Some(problem) = &problem {
        text.push_str("FAILED: ");
        text.push_str(problem);
        text.push_str("\nThe layers above it were not merged.\n");
    }
    if merged > 0 {
        text.push_str(
            "Next: run caos-std/stack-sync with remove-merged=true to drop the merged layers and adopt GitHub's rebase of the ones above, then caos-std/stack-publish.\n",
        );
    } else if problem.is_none() {
        text.push_str("Nothing to merge: every selected layer's pull request is already merged.\n");
    }
    site.complete(
        state,
        result_block(&site.call.id, &text, problem.is_some()),
        None,
        None,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack_publish::Description;

    fn layer(name: &str, described: bool) -> Layer {
        let (_, short) = crate::stack_publish::layer_name(name).unwrap();
        Layer {
            name: name.into(),
            short: short.into(),
            commit: Oid::parse(&"a".repeat(40), "test").unwrap(),
            description: described.then(|| Description {
                title: format!("Title of {name}"),
                body: String::new(),
            }),
        }
    }

    fn names(selected: &[&Layer]) -> Vec<&str> {
        selected.iter().map(|layer| layer.name.as_str()).collect()
    }

    #[test]
    fn up_to_selects_the_described_layers_below_and_including_it() {
        let layers = vec![
            layer("01-feature", true),
            layer("02-tests", true),
            layer("99-dirty", false),
        ];
        assert_eq!(names(&select(&layers, None).unwrap()), ["01-feature", "02-tests"]);
        assert_eq!(names(&select(&layers, Some("01-feature")).unwrap()), ["01-feature"]);
        assert_eq!(names(&select(&layers, Some("01")).unwrap()), ["01-feature"]);
        assert_eq!(names(&select(&layers, Some("tests")).unwrap()), ["01-feature", "02-tests"]);
        assert!(select(&layers, Some("99-dirty")).unwrap_err().contains("no .pr.md"));
        assert!(select(&layers, Some("03")).unwrap_err().contains("names no layer"));
        assert!(select(&[layer("99-dirty", false)], None).unwrap_err().contains("no layer has"));
    }

    #[test]
    fn the_commit_title_is_the_pr_title_and_number() {
        assert_eq!(commit_title(" Add the parser ", 365), "Add the parser (#365)");
    }

    #[test]
    fn a_pull_request_is_read_from_its_json() {
        let pr = pr_from(&json!({
            "number": 7, "draft": true, "merged_at": null,
            "head": {"sha": "abc"}, "base": {"ref": "main"},
            "stack": {"number": 9, "position": 1, "size": 2},
        }))
        .unwrap();
        assert_eq!((pr.number, pr.head.as_str(), pr.base.as_str()), (7, "abc", "main"));
        assert!(pr.draft && pr.stacked && !pr.merged);
        let pr = pr_from(&json!({
            "number": 8, "merged_at": "2026-10-30T00:00:00Z",
            "head": {"sha": "def"}, "base": {"ref": "feature"}, "stack": null,
        }))
        .unwrap();
        assert!(pr.merged && !pr.stacked && !pr.draft);
        assert!(pr_from(&json!({"number": 1})).is_none());
    }

    #[test]
    fn parameters_are_strings_with_defaults() {
        let call = |input: Value| Call {
            id: "a".into(),
            name: "run_tool".into(),
            input,
        };
        let p = parameters(&call(json!({"stack": "sources/repo/"}))).unwrap();
        assert_eq!(p.stack, "sources/repo");
        assert!(!p.force && !p.dry_run && p.up_to.is_none());
        let p = parameters(&call(json!({
            "stack": "sources/repo", "up-to": "02-tests", "force": "true",
            "dry-run": "true", "branch-prefix": "me/", "trunk": "main"
        })))
        .unwrap();
        assert!(p.force && p.dry_run);
        assert_eq!(p.up_to.as_deref(), Some("02-tests"));
        assert_eq!(p.branch_prefix.as_deref(), Some("me/"));
        for input in [
            json!({}),
            json!({"stack": ""}),
            json!({"stack": "sources/repo", "squash": "true"}),
            json!({"stack": "sources/repo", "trunk": "bad..name"}),
            json!({"stack": "sources/repo", "force": "maybe"}),
        ] {
            assert!(parameters(&call(input.clone())).is_err(), "{input}");
        }
    }
}
