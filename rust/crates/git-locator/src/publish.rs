//! Git publication receipts and remote branch lookup.
/// Render only known error codes; never relay Git stderr or a proxy error page.
pub fn diagnostic(code: &str) -> Option<&'static str> {
    Some(match code {
        "missing-commit" => "The server does not hold the source commit. Import it before publishing.",
        "missing-expected" => "The server does not hold the expected remote head. Import it before publishing.",
        "not-fast-forward" => "The update is not a fast-forward. Import and merge the remote head before publishing.",
        "validation-failed" => "The server could not validate the source commit; no push was attempted.",
        "lease-rejected" => "The remote head does not match the pinned lease. Inspect it before importing and integrating changes.",
        "hook-declined" => "The remote rejected the push: a receive hook declined it. Check repository rules and branch protection.",
        "remote-rejected" => "The remote rejected this branch update. Check repository rules and write access.",
        "invalid-request" => "The server rejected the publication request before pushing.",
        _ => return None,
    })
}

/// An absent branch is an observation, not a failed lookup.
pub fn read_branch(
    destination: &str,
    branch: &str,
    token: Option<&str>,
) -> Result<Option<String>, String> {
    let refname = format!("refs/heads/{branch}");
    let output = crate::import::git(destination, token)?
        .args(["ls-remote", "--refs", "--", destination, &refname])
        .output()
        .map_err(|_| "could not start remote branch lookup")?;
    if !output.status.success() {
        return Err("remote branch lookup failed".into());
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|_| "invalid Git response")?;
    let mut found = None;
    for line in text.lines() {
        let (hash, name) = line
            .split_once('\t')
            .ok_or("invalid remote branch response")?;
        if name != refname || !crate::import::commit(hash) || found.is_some() {
            return Err("invalid remote branch response".into());
        }
        found = Some(hash.to_ascii_lowercase());
    }
    Ok(found)
}
