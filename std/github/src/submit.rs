//! Publish pinned Git objects through CAOS; use gh only for PR metadata.
use conversation_protocol::v3::stacks::{Publication, Submission};
use serde_json::{json, Value};

pub trait Backend {
    fn push(
        &mut self,
        repository: &str,
        layer: &Publication,
        rewrite: bool,
    ) -> Result<Value, String>;
    fn gh(&mut self, args: &[String], stdin: &str) -> Result<Value, String>;
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).into()).collect()
}

/// The outer invocation claim covers this whole sequence. A retry reports an
/// unfinished claim as uncertain; a NEW invocation observes branches and PRs
/// again and can finish a partially submitted stack.
pub fn run(plan: &Submission, backend: &mut impl Backend) -> Value {
    let mut receipts = Vec::new();
    let mut urls = Vec::new();
    let mut base = plan.base_branch.clone();
    let execute = (|| -> Result<(), Value> {
        if plan.layers.is_empty() {
            return Err(
                json!({"status":"complete","exit":1,"stderr":"a stack needs at least one layer"}),
            );
        }
        for layer in &plan.layers {
            let pushed = backend
                .push(&plan.repository, layer, plan.rewrite)
                .map_err(|e| json!({"status":"uncertain","exit":null,"stderr":e}))?;
            receipts.push(json!({"branch":layer.branch,"commit":layer.commit,"push":pushed}));
            if pushed["status"] != "complete" {
                return Err(
                    json!({"status":if pushed["status"]=="uncertain"{"uncertain"}else{"complete"},
                    "exit":1,"stderr":pushed["diagnostic"]}),
                );
            }
            let mut gh = |args: Vec<String>, stdin: &str| command(backend, &args, stdin);
            let listed = gh(
                strings(&[
                    "pr",
                    "list",
                    "--repo",
                    &plan.repository,
                    "--state",
                    "open",
                    "--head",
                    &layer.branch,
                    "--json",
                    "url,baseRefName,headRepositoryOwner",
                ]),
                "",
            )?;
            let prs: Vec<Value> = serde_json::from_str(&listed)
                .map_err(|_| json!({"status":"complete","exit":1,"stderr":"invalid PR listing"}))?;
            let owner = plan.repository.split('/').next().unwrap_or("");
            let prs: Vec<_> = prs
                .iter()
                .filter(|pr| {
                    pr["headRepositoryOwner"]["login"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(owner))
                })
                .collect();
            if prs.len() > 1 {
                return Err(
                    json!({"status":"complete","exit":1,"stderr":"multiple open PRs match the stack branch"}),
                );
            }
            let url = if let Some(pr) = prs.first() {
                let url = pr["url"]
                    .as_str()
                    .ok_or_else(|| json!({"status":"complete","exit":1,"stderr":"PR lacks URL"}))?;
                if pr["baseRefName"] != base {
                    gh(
                        strings(&[
                            "pr",
                            "edit",
                            url,
                            "--repo",
                            &plan.repository,
                            "--base",
                            &base,
                        ]),
                        "",
                    )?;
                }
                url.to_owned()
            } else {
                let mut args = strings(&[
                    "pr",
                    "create",
                    "--repo",
                    &plan.repository,
                    "--head",
                    &layer.branch,
                    "--base",
                    &base,
                    "--title",
                    &layer.title,
                    "--body-file",
                    "-",
                ]);
                if plan.draft {
                    args.push("--draft".into());
                }
                gh(args, &layer.body)?.trim().to_owned()
            };
            if !url.to_ascii_lowercase().starts_with(&format!(
                "https://github.com/{}/pull/",
                plan.repository.to_ascii_lowercase()
            )) {
                return Err(json!({"status":"uncertain","exit":null,"stderr":"unexpected PR URL"}));
            }
            receipts.last_mut().unwrap()["pr"] = json!(url);
            urls.push(url);
            base = layer.branch.clone();
        }
        if urls.len() > 1 {
            link_stack(&plan.repository, &urls, backend)?;
        }
        Ok(())
    })();
    let mut result = match execute {
        Ok(()) => json!({"status":"complete","exit":0,"stderr":""}),
        Err(result) => result,
    };
    result["stdout"] =
        json!(serde_json::to_string(&json!({"branches":receipts,"pull_requests":urls})).unwrap());
    result
}

fn command(backend: &mut impl Backend, args: &[String], stdin: &str) -> Result<String, Value> {
    let value = backend
        .gh(args, stdin)
        .map_err(|e| json!({"status":"uncertain","exit":null,"stderr":e}))?;
    if value["status"] != "complete" || value["exit"] != 0 {
        return Err(value);
    }
    value["stdout"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| json!({"status":"uncertain","exit":null,"stderr":"invalid GitHub result"}))
}

fn link_stack(repository: &str, urls: &[String], backend: &mut impl Backend) -> Result<(), Value> {
    let fail = |message: &str| json!({"status":"complete","exit":1,"stderr":message});
    let numbers = urls
        .iter()
        .map(|url| url.rsplit('/').next().unwrap_or("").parse::<u64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| fail("invalid PR URL"))?;
    let endpoint = format!("repos/{repository}/stacks");
    let text = command(
        backend,
        &strings(&[
            "api",
            &format!("{endpoint}?per_page=100"),
            "--paginate",
            "--slurp",
        ]),
        "",
    )?;
    let pages: Vec<Vec<Value>> =
        serde_json::from_str(&text).map_err(|_| fail("invalid GitHub stack listing"))?;
    let matching: Vec<_> = pages
        .iter()
        .flatten()
        .filter(|stack| {
            stack["pull_requests"].as_array().is_some_and(|prs| {
                prs.iter()
                    .any(|pr| pr["number"].as_u64().is_some_and(|n| numbers.contains(&n)))
            })
        })
        .collect();
    if matching.len() > 1 {
        return Err(fail(
            "PRs belong to different GitHub stacks; reconcile their membership first",
        ));
    }
    let (endpoint, additions) = if let Some(stack) = matching.first() {
        let prs = stack["pull_requests"]
            .as_array()
            .ok_or_else(|| fail("invalid GitHub stack"))?;
        // Keep merged entries on GitHub. gh-stack v0.1.1 link requires them in
        // its input yet refuses merged PRs, so it cannot extend a landed stack.
        let active = prs
            .iter()
            .filter(|pr| pr["merged_at"].is_null())
            .map(|pr| {
                pr["number"]
                    .as_u64()
                    .ok_or_else(|| fail("invalid stack PR number"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !numbers.starts_with(&active) {
            return Err(fail("existing stack members are not a prefix of this submission; reordering or removing members requires an explicit GitHub operation"));
        }
        if numbers.len() == active.len() {
            return Ok(());
        }
        let number = stack["number"]
            .as_u64()
            .ok_or_else(|| fail("invalid GitHub stack number"))?;
        (format!("{endpoint}/{number}/add"), &numbers[active.len()..])
    } else {
        (endpoint, numbers.as_slice())
    };
    command(
        backend,
        &strings(&["api", "--method", "POST", &endpoint, "--input", "-"]),
        &json!({"pull_requests":additions}).to_string(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use conversation_protocol::v3::Oid;
    struct Fake {
        calls: Vec<Vec<String>>,
        old: bool,
        fail_push: bool,
        landed_prefix: bool,
        inputs: Vec<String>,
    }
    impl Backend for Fake {
        fn push(&mut self, _: &str, layer: &Publication, rewrite: bool) -> Result<Value, String> {
            self.calls.push(vec![
                "push".into(),
                layer.branch.clone(),
                rewrite.to_string(),
                layer
                    .expected
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            ]);
            Ok(if self.fail_push && layer.branch == "feature/b" {
                json!({"status":"conflict","diagnostic":"lease rejected"})
            } else {
                json!({"status":"complete"})
            })
        }
        fn gh(&mut self, args: &[String], input: &str) -> Result<Value, String> {
            self.calls.push(args.to_vec());
            self.inputs.push(input.into());
            let stdout = if args[0] == "api" {
                if args.contains(&"--method".into()) {
                    "{}".into()
                } else if self.landed_prefix {
                    json!([[{"number":7,"pull_requests":[
                        {"number":9,"merged_at":"2026-01-01"},
                        {"number":1,"merged_at":null}]}]])
                    .to_string()
                } else if self.old {
                    json!([[{"number":7,"pull_requests":[
                        {"number":1,"merged_at":null},{"number":2,"merged_at":null}]}]])
                    .to_string()
                } else {
                    "[[]]".into()
                }
            } else if args[1] == "list" {
                let branch = &args[args.iter().position(|s| s == "--head").unwrap() + 1];
                if self.old {
                    json!([{"url":format!("https://github.com/o/r/pull/{}",if branch=="feature/a"{1}else{2}),
                    "baseRefName":"main","headRepositoryOwner":{"login":"o"}}]).to_string()
                } else {
                    "[]".into()
                }
            } else if args[1] == "create" {
                let branch = &args[args.iter().position(|s| s == "--head").unwrap() + 1];
                format!(
                    "https://github.com/o/r/pull/{}",
                    if branch == "feature/a" { 1 } else { 2 }
                )
            } else {
                String::new()
            };
            Ok(json!({"status":"complete","exit":0,"stdout":stdout,"stderr":""}))
        }
    }
    fn plan() -> Submission {
        Submission {
            repository: "o/r".into(),
            base_branch: "main".into(),
            rewrite: true,
            draft: false,
            layers: ["feature/a", "feature/b"]
                .into_iter()
                .map(|branch| Publication {
                    branch: branch.into(),
                    commit: Oid::parse(&"a".repeat(40), "commit").unwrap(),
                    expected: Some(Oid::parse(&"b".repeat(40), "old").unwrap()),
                    title: "A change".into(),
                    body: "Description".into(),
                })
                .collect(),
        }
    }
    #[test]
    fn first_submission_creates_ordered_prs_and_stack() {
        let mut backend = Fake {
            calls: Vec::new(),
            old: false,
            fail_push: false,
            landed_prefix: false,
            inputs: Vec::new(),
        };
        let result = run(&plan(), &mut backend);
        assert_eq!(result["exit"], 0);
        let creates: Vec<_> = backend
            .calls
            .iter()
            .filter(|c| c.get(1).is_some_and(|s| s == "create"))
            .collect();
        assert_eq!(creates.len(), 2);
        assert!(creates[1].windows(2).any(|w| w == ["--base", "feature/a"]));
        assert!(!creates[0].contains(&"--draft".into()));
        assert_eq!(
            backend.calls.last().unwrap(),
            &strings(&[
                "api",
                "--method",
                "POST",
                "repos/o/r/stacks",
                "--input",
                "-"
            ])
        );
        assert_eq!(
            serde_json::from_str::<Value>(backend.inputs.last().unwrap()).unwrap(),
            json!({"pull_requests":[1,2]})
        );
    }
    #[test]
    fn resubmission_reuses_prs_and_preserves_titles_and_bodies() {
        let mut backend = Fake {
            calls: Vec::new(),
            old: true,
            fail_push: false,
            landed_prefix: false,
            inputs: Vec::new(),
        };
        let result = run(&plan(), &mut backend);
        assert_eq!(result["exit"], 0);
        assert!(!backend
            .calls
            .iter()
            .any(|c| c.get(1).is_some_and(|s| s == "create")));
        assert!(!backend
            .calls
            .iter()
            .any(|c| c.contains(&"--title".into()) || c.contains(&"--body-file".into())));
        assert!(backend.calls.iter().any(|c| c
            == &strings(&[
                "pr",
                "edit",
                "https://github.com/o/r/pull/2",
                "--repo",
                "o/r",
                "--base",
                "feature/a"
            ])));
        assert!(backend
            .calls
            .iter()
            .filter(|c| c[0] == "push")
            .all(|c| c[2] == "true" && c[3] == "b".repeat(40)));
    }
    #[test]
    fn failed_upper_push_keeps_lower_receipt_and_does_not_link_a_partial_stack() {
        let mut backend = Fake {
            calls: Vec::new(),
            old: false,
            fail_push: true,
            landed_prefix: false,
            inputs: Vec::new(),
        };
        let result = run(&plan(), &mut backend);
        assert_eq!(result["exit"], 1);
        assert!(!backend.calls.iter().any(|c| c[0] == "api"));
        let receipts: Value = serde_json::from_str(result["stdout"].as_str().unwrap()).unwrap();
        assert_eq!(
            receipts["branches"][0]["pr"],
            "https://github.com/o/r/pull/1"
        );
        assert_eq!(receipts["branches"][1]["push"]["status"], "conflict");
    }
    #[test]
    fn extends_stack_after_lower_prs_have_merged() {
        let mut backend = Fake {
            calls: Vec::new(),
            old: true,
            fail_push: false,
            landed_prefix: true,
            inputs: Vec::new(),
        };
        let result = run(&plan(), &mut backend);
        assert_eq!(result["exit"], 0);
        assert_eq!(
            backend.calls.last().unwrap(),
            &strings(&[
                "api",
                "--method",
                "POST",
                "repos/o/r/stacks/7/add",
                "--input",
                "-"
            ])
        );
        assert_eq!(
            serde_json::from_str::<Value>(backend.inputs.last().unwrap()).unwrap(),
            json!({"pull_requests":[2]})
        );
    }
}
