//! Git publication checks shared by the server and host publisher.
use std::process::Output;

pub fn reject_caos(
    commit: &str,
    capture: impl Fn(&[&str]) -> Result<String, String>,
) -> Result<(), String> {
    let listing = capture(&["ls-tree", "-r", "--name-only", commit, "--", ".caos"])?;
    if listing.lines().any(|path| path == ".caos/conflicts") {
        let contents = capture(&["show", &format!("{commit}:{}", ".caos/conflicts")])?;
        return Err(if contents.trim().is_empty() {
            "the source tree has an empty `.caos/conflicts` file; remove it with bash before publishing (the removal is committed automatically)".into()
        } else {
            "the source tree has unresolved `.caos/conflicts` entries; resolve the listed paths and clear their ledger entries; saving the resolution removes empty merge metadata".into()
        });
    }
    let root = capture(&["ls-tree", "--name-only", commit, "--", ".caos"])?;
    if root.trim().is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the source tree contains reserved `.caos` content; record a cleaned source-tree edit before publishing:\n{}",
            if listing.trim().is_empty() { ".caos/ (empty directory)" } else { listing.trim_end() }
        ))
    }
}

pub fn reject_markers(
    commit: &str,
    run: impl Fn(&[&str]) -> Result<Output, String>,
) -> Result<(), String> {
    let result = run(&[
        "grep",
        "-I",
        "-n",
        "-e",
        "^<<<<<<< ",
        "-e",
        "^=======$",
        "-e",
        "^>>>>>>> ",
        commit,
        "--",
    ])?;
    match result.status.code() {
        Some(1) => Ok(()),
        Some(0) => Err("source tree contains unresolved merge markers".into()),
        _ => Err("could not check source tree for merge markers".into()),
    }
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
