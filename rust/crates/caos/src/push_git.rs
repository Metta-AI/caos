//! Worker-side client for a pinned, leased branch publication.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut positional = Vec::new();
    let mut token_file = None;
    let mut expected = None;
    let mut rewrite = false;
    for arg in args {
        if let Some(value) = arg.strip_prefix("--github-token-file=") {
            if token_file.replace(value).is_some() {
                return Err("duplicate token file".into());
            }
        } else if let Some(value) = arg.strip_prefix("--expected=") {
            if expected.replace(value).is_some() {
                return Err("duplicate expected head".into());
            }
        } else if arg == "--rewrite" {
            if rewrite {
                return Err("duplicate rewrite flag".into());
            }
            rewrite = true;
        } else if arg.starts_with('-') {
            return Err("unknown push-git option".into());
        } else {
            positional.push(arg.as_str());
        }
    }
    let [destination, commit, branch] = positional.as_slice() else {
        return Err("push-git requires an HTTPS repository, full commit hash, branch and --expected=<hash|absent>".into());
    };
    git_locator::import::remote(destination)?;
    if !git_locator::import::commit(commit) {
        return Err("push-git requires a full commit hash".into());
    }
    let expected = match expected {
        Some("absent") => None,
        Some(hash) if git_locator::import::commit(hash) => Some(hash.to_ascii_lowercase()),
        _ => return Err("push-git requires --expected=<full hash|absent>".into()),
    };
    let commit = commit.to_ascii_lowercase();
    let body = serde_json::json!({"destination":destination,"commit":commit,"branch":branch,"expected":expected,"rewrite":rewrite}).to_string();
    // Everything that can fail locally happens before dispatch. Once a request
    // may have been sent, always return a receipt, even for an unreadable response.
    let (server, headers) = crate::import_git::prepare(destination, token_file)?;
    let response = crate::import_git::post(&server, "/git/push", &body, &headers);
    println!("{}", receipt(&commit, branch, response));
    Ok(())
}

fn receipt(
    commit: &str,
    branch: &str,
    response: Result<crate::ServerResponse, String>,
) -> serde_json::Value {
    use serde_json::{json, Value};
    let uncertain = || {
        json!({
            "commit":commit, "branch":branch, "status":"uncertain", "observed":null,
            "kind":"ambiguous",
            "diagnostic":"No confirmed push result. Inspect the remote; keep the pinned commit and lease."
        })
    };
    let Ok(response) = response else {
        return uncertain();
    };
    let value: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    let diagnostic = |code: &str| git_locator::publish::diagnostic(code).map(str::to_owned);
    if matches!(response.status, 400 | 422) {
        let message = value["code"]
            .as_str()
            .and_then(diagnostic)
            .unwrap_or_else(|| {
                git_locator::publish::diagnostic("invalid-request")
                    .unwrap()
                    .into()
            });
        return json!({"commit":commit, "branch":branch, "status":"conflict",
            "observed":null, "kind":"validation-rejected", "diagnostic":message});
    }
    if response.status != 200
        || value["commit"].as_str() != Some(commit)
        || value["branch"].as_str() != Some(branch)
        || !value
            .get("observed")
            .is_some_and(|v| v.is_null() || v.as_str().is_some_and(git_locator::import::commit))
        || !matches!(
            (value["status"].as_str(), value["kind"].as_str()),
            (Some("complete"), Some("push-success" | "ref-converged"))
                | (
                    Some("conflict"),
                    Some("lease-rejected" | "ref-drift" | "push-rejected")
                )
                | (Some("uncertain"), Some("ambiguous"))
        )
    {
        return uncertain();
    }
    // Reconstruct the receipt; discard arbitrary text and unrecognized fields.
    json!({"commit":commit, "branch":branch, "status":value["status"],
        "observed":value["observed"], "kind":value["kind"],
        "diagnostic":value["code"].as_str().and_then(diagnostic)})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn distinguishes_rejections_from_missing_or_untrusted_responses() {
        let h = "a".repeat(40);
        let response = |status, body: String| {
            Ok(crate::ServerResponse {
                status,
                reason: String::new(),
                body: body.into_bytes(),
            })
        };
        let r = receipt(
            &h,
            "topic",
            response(422, json!({"code":"missing-expected"}).to_string()),
        );
        assert_eq!(r["kind"], "validation-rejected");
        assert!(r["diagnostic"].as_str().unwrap().contains("Import"));
        let r = receipt(&h, "topic", response(422, "sensitive proxy text".into()));
        assert!(!r.to_string().contains("sensitive"));
        for r in [
            receipt(&h, "topic", Err("transport".into())),
            receipt(&h, "topic", response(200, "not JSON".into())),
            receipt(&h, "topic", response(502, "sensitive proxy text".into())),
        ] {
            assert_eq!(r["status"], "uncertain");
            assert!(!r.to_string().contains("sensitive"));
        }
    }
}
