//! What CI says about a commit, read from GitHub: its check runs and its
//! commit statuses, and the verdict `stack-merge` gates on
//! (design/stack-publish.md, "ci-status"). `ci-status` is the operation that
//! prints it for a branch, a layer or a commit; `stack_merge` reads the same
//! thing for every pull request head before it merges anything.
use super::*;

use crate::stack_publish::{github_repo, provenance, read_stack, Github};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    branch: String,
    #[serde(default)]
    stack: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default, rename = "branch-prefix", alias = "branch_prefix")]
    branch_prefix: Option<String>,
}

fn blank(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

/// A reference GitHub can take in a URL path: a branch, a layer or a commit.
fn validate_reference(reference: &str) -> Result<(), String> {
    let plain = |c: char| c.is_ascii_alphanumeric() || "-_./".contains(c);
    if reference.is_empty() || !reference.chars().all(plain) || reference.contains("..") {
        return Err(format!(
            "branch must be a branch name, a layer name or a commit hash, not {reference:?}"
        ));
    }
    Ok(())
}

fn parameters(call: &Call) -> Result<Parameters, String> {
    let mut p: Parameters = serde_json::from_value(call.input.clone())
        .map_err(|error| format!("invalid ci-status arguments: {error}"))?;
    p.branch = p.branch.trim().to_string();
    validate_reference(&p.branch)?;
    p.stack = blank(p.stack).map(|stack| stack.trim().trim_matches('/').to_string());
    if let Some(stack) = &p.stack {
        paths::validate_source_tree_name(stack)?;
    }
    p.repository = blank(p.repository);
    if let Some(repository) = &p.repository {
        git_locator::import::remote(repository)?;
    }
    p.branch_prefix = blank(p.branch_prefix);
    if p.stack.is_none() && p.repository.is_none() {
        return Err("pass repository, or stack to take it from 00-base.source.json".into());
    }
    Ok(p)
}

/// The GitHub token the step is granted, when the repository is on github.com.
pub(super) fn token_for(repository: &str) -> Result<Option<String>, String> {
    import_source::token_file(repository)
        .map(|path| fs::read_to_string(path).map(|text| text.trim_end().to_string()))
        .transpose()
        .map_err(|_| "reading GitHub token".to_string())
}

/// Where one check run or commit status stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Standing {
    Passing,
    Pending,
    Failing,
}

/// A check run's standing, from its `status` and `conclusion`. Neutral and
/// skipped pass: they are how a check says it had nothing to object to. Any
/// other completed conclusion (failure, cancelled, timed_out, action_required,
/// stale, startup_failure) fails, and a run that has not completed is pending.
pub(super) fn run_standing(status: &str, conclusion: Option<&str>) -> Standing {
    if status != "completed" {
        return Standing::Pending;
    }
    match conclusion {
        Some("success") | Some("neutral") | Some("skipped") => Standing::Passing,
        _ => Standing::Failing,
    }
}

/// A commit status's standing, from its `state`: success, pending, or one of
/// the two that fail (failure, error).
pub(super) fn status_standing(state: &str) -> Standing {
    match state {
        "success" => Standing::Passing,
        "pending" => Standing::Pending,
        _ => Standing::Failing,
    }
}

#[derive(Clone, Debug)]
pub(super) struct Check {
    pub name: String,
    /// The conclusion of a completed run, else its status; a status's state.
    pub state: String,
    pub standing: Standing,
    pub url: String,
}

/// Every check run and commit status of one commit.
#[derive(Clone, Debug)]
pub(super) struct Checks {
    pub head: String,
    pub checks: Vec<Check>,
}

impl Checks {
    /// Failing if anything fails, else pending if anything has not finished,
    /// else passing. None when the commit has no checks at all.
    pub(super) fn verdict(&self) -> Option<Standing> {
        if self.checks.is_empty() {
            return None;
        }
        let any = |standing: Standing| self.checks.iter().any(|check| check.standing == standing);
        Some(if any(Standing::Failing) {
            Standing::Failing
        } else if any(Standing::Pending) {
            Standing::Pending
        } else {
            Standing::Passing
        })
    }

    fn named(&self, standing: Standing) -> Vec<&str> {
        self.checks
            .iter()
            .filter(|check| check.standing == standing)
            .map(|check| check.name.as_str())
            .collect()
    }

    /// One phrase: `no checks`, `3 passing`, `pending: build (2 passing)`,
    /// `FAILING: test; pending: lint`.
    pub(super) fn summary(&self) -> String {
        let passing = self.named(Standing::Passing).len();
        let pending = self.named(Standing::Pending);
        let failing = self.named(Standing::Failing);
        match self.verdict() {
            None => "no checks".to_string(),
            Some(Standing::Passing) => format!("{passing} passing"),
            Some(Standing::Pending) => {
                format!("pending: {} ({passing} passing)", pending.join(", "))
            }
            Some(Standing::Failing) => {
                let mut text = format!("FAILING: {}", failing.join(", "));
                if !pending.is_empty() {
                    text.push_str(&format!("; pending: {}", pending.join(", ")));
                }
                text
            }
        }
    }
}

/// Read the check runs and commit statuses of `reference` (a branch, a tag or
/// a commit hash), resolving it to a commit first.
pub(super) fn read(github: &Github, reference: &str) -> Result<Checks, String> {
    let commit = github.expect(
        "GET",
        &github.repo_path(&format!("/commits/{reference}")),
        None,
        &[200],
    )?;
    let head = commit["sha"]
        .as_str()
        .ok_or_else(|| format!("GitHub returned no commit for {reference}"))?
        .to_string();
    let mut checks = Vec::new();
    let mut page = 1;
    loop {
        let runs = github.expect(
            "GET",
            &github.repo_path(&format!("/commits/{head}/check-runs?per_page=100&page={page}")),
            None,
            &[200],
        )?;
        let batch = runs["check_runs"].as_array().cloned().unwrap_or_default();
        let total = runs["total_count"].as_u64().unwrap_or(0) as usize;
        for run in &batch {
            let status = run["status"].as_str().unwrap_or("");
            let conclusion = run["conclusion"].as_str();
            checks.push(Check {
                name: run["name"].as_str().unwrap_or("?").to_string(),
                state: conclusion.unwrap_or(status).to_string(),
                standing: run_standing(status, conclusion),
                url: run["html_url"]
                    .as_str()
                    .or(run["details_url"].as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
        if batch.is_empty() || checks.len() >= total || page >= 10 {
            break;
        }
        page += 1;
    }
    let combined = github.expect(
        "GET",
        &github.repo_path(&format!("/commits/{head}/status")),
        None,
        &[200],
    )?;
    for status in combined["statuses"].as_array().into_iter().flatten() {
        let state = status["state"].as_str().unwrap_or("");
        checks.push(Check {
            name: status["context"].as_str().unwrap_or("?").to_string(),
            state: state.to_string(),
            standing: status_standing(state),
            url: status["target_url"].as_str().unwrap_or("").to_string(),
        });
    }
    Ok(Checks { head, checks })
}

pub(super) fn render(repository: &str, reference: &str, checks: &Checks) -> String {
    let mut text = format!(
        "repository {repository}  branch {reference}  head {}\n",
        &checks.head[..checks.head.len().min(12)]
    );
    for check in &checks.checks {
        text.push_str(&format!("{:<16} {}", check.state, check.name));
        if !check.url.is_empty() {
            text.push_str("  ");
            text.push_str(&check.url);
        }
        text.push('\n');
    }
    text.push_str(&format!("checks: {}\n", checks.summary()));
    text
}

/// The repository and the reference to read, from the parameters and, when a
/// stack is named, its provenance and layers: a layer's name, or its name
/// without the number, means that layer's branch.
fn resolve(view: &Conversation<'_>, p: &Parameters) -> Result<(String, String), String> {
    let Some(stack) = &p.stack else {
        let repository = p.repository.clone().ok_or("no repository")?;
        return Ok((repository, p.branch.clone()));
    };
    let prov = provenance(view, stack)?;
    let repository = p
        .repository
        .clone()
        .or_else(|| prov.repository.clone())
        .ok_or_else(|| {
            format!("no repository: {stack}/00-base.source.json names none, so pass repository")
        })?;
    git_locator::import::remote(&repository)?;
    let layers = read_stack(view, stack)?.layers;
    let prefix = p.branch_prefix.clone().unwrap_or_default();
    let reference = match layers
        .iter()
        .find(|layer| layer.name == p.branch || layer.short == p.branch)
    {
        Some(layer) => format!("{prefix}{}", layer.short),
        None => p.branch.clone(),
    };
    Ok((repository, reference))
}

pub(super) fn execute(state: &mut progress::State, site: &CallSite<'_>) -> Result<(), String> {
    let p = match parameters(site.call) {
        Ok(p) => p,
        Err(error) => return site.fail(state, &error),
    };
    let resolved = resolve(&state.conversation()?, &p);
    let (repository, reference) = match resolved {
        Ok(resolved) => resolved,
        Err(error) => return site.fail(state, &format!("ci-status refused: {error}")),
    };
    let Some((owner, repo)) = github_repo(&repository) else {
        return site.fail(
            state,
            &format!(
                "ci-status: {repository} is not on github.com, which is where check runs are read"
            ),
        );
    };
    let github = Github {
        owner,
        repo,
        token: token_for(&repository)?,
    };
    match read(&github, &reference) {
        Ok(checks) => {
            let block = result_block(&site.call.id, &render(&repository, &reference, &checks), false);
            site.complete(state, block, None, None, None)
        }
        Err(error) => site.fail(state, &format!("ci-status: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, standing: Standing) -> Check {
        Check {
            name: name.into(),
            state: String::new(),
            standing,
            url: String::new(),
        }
    }

    #[test]
    fn a_run_passes_only_when_it_completed_well() {
        assert_eq!(run_standing("completed", Some("success")), Standing::Passing);
        assert_eq!(run_standing("completed", Some("neutral")), Standing::Passing);
        assert_eq!(run_standing("completed", Some("skipped")), Standing::Passing);
        assert_eq!(run_standing("completed", Some("failure")), Standing::Failing);
        assert_eq!(run_standing("completed", Some("cancelled")), Standing::Failing);
        assert_eq!(run_standing("completed", Some("timed_out")), Standing::Failing);
        assert_eq!(run_standing("completed", Some("action_required")), Standing::Failing);
        assert_eq!(run_standing("completed", None), Standing::Failing);
        assert_eq!(run_standing("in_progress", None), Standing::Pending);
        assert_eq!(run_standing("queued", None), Standing::Pending);
        assert_eq!(status_standing("success"), Standing::Passing);
        assert_eq!(status_standing("pending"), Standing::Pending);
        assert_eq!(status_standing("failure"), Standing::Failing);
        assert_eq!(status_standing("error"), Standing::Failing);
    }

    #[test]
    fn the_verdict_is_the_worst_standing_and_the_summary_names_it() {
        let head = "0123456789abcdef".to_string();
        let none = Checks {
            head: head.clone(),
            checks: Vec::new(),
        };
        assert_eq!(none.verdict(), None);
        assert_eq!(none.summary(), "no checks");
        let green = Checks {
            head: head.clone(),
            checks: vec![check("build", Standing::Passing), check("test", Standing::Passing)],
        };
        assert_eq!(green.verdict(), Some(Standing::Passing));
        assert_eq!(green.summary(), "2 passing");
        let waiting = Checks {
            head: head.clone(),
            checks: vec![check("build", Standing::Passing), check("test", Standing::Pending)],
        };
        assert_eq!(waiting.verdict(), Some(Standing::Pending));
        assert_eq!(waiting.summary(), "pending: test (1 passing)");
        let red = Checks {
            head,
            checks: vec![
                check("build", Standing::Failing),
                check("test", Standing::Pending),
                check("lint", Standing::Passing),
            ],
        };
        assert_eq!(red.verdict(), Some(Standing::Failing));
        assert_eq!(red.summary(), "FAILING: build; pending: test");
        assert!(render("https://github.com/o/r", "feature", &red).starts_with(
            "repository https://github.com/o/r  branch feature  head 0123456789ab\n"
        ));
    }

    #[test]
    fn parameters_need_a_repository_or_a_stack() {
        let call = |input: Value| Call {
            id: "a".into(),
            name: "run_tool".into(),
            input,
        };
        let p = parameters(&call(json!({"branch": "feature", "repository": "https://github.com/o/r"})))
            .unwrap();
        assert_eq!(p.branch, "feature");
        assert_eq!(p.stack, None);
        let p = parameters(&call(json!({"branch": "02-tests", "stack": "sources/repo/", "branch-prefix": "me/"})))
            .unwrap();
        assert_eq!(p.stack.as_deref(), Some("sources/repo"));
        assert_eq!(p.branch_prefix.as_deref(), Some("me/"));
        for input in [
            json!({}),
            json!({"branch": "feature"}),
            json!({"branch": "", "repository": "https://github.com/o/r"}),
            json!({"branch": "a b", "repository": "https://github.com/o/r"}),
            json!({"branch": "a?x=1", "repository": "https://github.com/o/r"}),
            json!({"branch": "feature", "repository": "git@github.com:o/r"}),
            json!({"branch": "feature", "stack": "sources/repo", "extra": "x"}),
        ] {
            assert!(parameters(&call(input.clone())).is_err(), "{input}");
        }
    }
}
