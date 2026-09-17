//! Worker-side client for a pinned, leased branch publication.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut positional = Vec::new();
    let mut token_file = None;
    let mut expected = None;
    for arg in args {
        if let Some(value) = arg.strip_prefix("--github-token-file=") {
            if token_file.replace(value).is_some() {
                return Err("duplicate token file".into());
            }
        } else if let Some(value) = arg.strip_prefix("--expected=") {
            if expected.replace(value).is_some() {
                return Err("duplicate expected head".into());
            }
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
    let body = serde_json::json!({"destination":destination,"commit":commit,"branch":branch,"expected":expected}).to_string();
    let response = crate::import_git::send(destination, "/git/push", &body, token_file)?;
    if response.status == 422 {
        // A validation rejection happens before any push. Do not confuse it
        // with a lost response, or echo an arbitrary proxy's response body.
        println!(
            "{}",
            serde_json::json!({
                "commit":commit, "branch":branch, "status":"conflict",
                "observed":null, "kind":"validation-rejected",
                "diagnostic":"The server rejected this commit before pushing. Check complete code history, unresolved conflicts or .caos files, and that the expected remote head is an ancestor."
            })
        );
        return Ok(());
    }
    if response.status != 200 {
        return Err(format!(
            "Git push request failed (HTTP {})",
            response.status
        ));
    }
    let response: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| "invalid publication response")?;
    // Preserve typed outcomes for callers; conflict and uncertainty are not job
    // failures and must not invite a fresh lease on automatic retry.
    if response["commit"].as_str() != Some(&commit)
        || response["branch"].as_str() != Some(branch)
        || !matches!(
            response["status"].as_str(),
            Some("complete" | "conflict" | "uncertain")
        )
        || !response
            .get("observed")
            .is_some_and(|v| v.is_null() || v.as_str().is_some_and(git_locator::import::commit))
    {
        return Err("invalid publication response".into());
    }
    println!("{response}");
    Ok(())
}
