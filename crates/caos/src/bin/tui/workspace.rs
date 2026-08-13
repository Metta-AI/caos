//! Local publication of a conversation workspace as one clean PR commit.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

pub(crate) fn publish_conversation_pr(
    name: &str,
    head: &str,
    cwd: &Path,
) -> Result<String, String> {
    refuse_unresolved_conflicts(head, cwd)?;
    let base = remote_default_branch(cwd)?;
    let base_commit = fetch_remote_branch_tip(&base, cwd)?;
    let merged_tree = merge_tree(head, &base_commit, cwd)?;
    let clean_tree = without_caos(&merged_tree, cwd)?;
    let branch = format!("caos/{name}");
    let branch_ref = format!("refs/heads/{branch}");
    let commit = capture_required(
        "git",
        &[
            "commit-tree",
            &clean_tree,
            "-p",
            &base_commit,
            "-m",
            &format!("CAOS conversation {name} at {}", short_hash(head)),
        ],
        cwd,
    )?;
    capture_required("git", &["update-ref", &branch_ref, &commit], cwd)?;

    let remote_tip = remote_branch_tip(&branch_ref, cwd)?;
    let lease = format!(
        "--force-with-lease={branch_ref}:{}",
        remote_tip.as_deref().unwrap_or("")
    );
    let refspec = format!("{branch_ref}:{branch_ref}");
    capture_required(
        "git",
        &["push", "--set-upstream", &lease, "origin", &refspec],
        cwd,
    )?;

    let existing = capture_required(
        "gh",
        &[
            "pr",
            "list",
            "--head",
            &branch,
            "--base",
            &base,
            "--state",
            "open",
            "--json",
            "url",
            "--jq",
            ".[0].url // empty",
        ],
        cwd,
    )?;
    if !existing.is_empty() {
        return Ok(existing);
    }
    capture_required(
        "gh",
        &[
            "pr",
            "create",
            "--head",
            &branch,
            "--base",
            &base,
            "--title",
            &format!("CAOS conversation {name}"),
            "--body",
            &format!(
                "Published from CAOS conversation `{name}` at `{}`.",
                short_hash(head)
            ),
        ],
        cwd,
    )
}

fn refuse_unresolved_conflicts(head: &str, cwd: &Path) -> Result<(), String> {
    let output = command_output("git", &["show", &format!("{head}:.caos/conflicts")], cwd)?;
    if output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty() {
        return Err("conversation still has unresolved merge conflicts".to_string());
    }
    Ok(())
}

fn remote_default_branch(cwd: &Path) -> Result<String, String> {
    let output = command_output("git", &["ls-remote", "--symref", "origin", "HEAD"], cwd)?;
    let output = require_success("git ls-remote", output)?;
    for line in String::from_utf8_lossy(&output).lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() == 3 && fields[0] == "ref:" && fields[2] == "HEAD" {
            return fields[1]
                .strip_prefix("refs/heads/")
                .filter(|branch| !branch.is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("origin HEAD points outside refs/heads: {}", fields[1]));
        }
    }
    Err("origin HEAD did not advertise a default branch".to_string())
}

fn fetch_remote_branch_tip(branch: &str, cwd: &Path) -> Result<String, String> {
    let remote_ref = format!("refs/heads/{branch}");
    let tracking_ref = format!("refs/remotes/origin/{branch}");
    let refspec = format!("+{remote_ref}:{tracking_ref}");
    capture_required(
        "git",
        &["fetch", "--quiet", "--no-tags", "origin", &refspec],
        cwd,
    )?;
    capture_required("git", &["rev-parse", "--verify", &tracking_ref], cwd)
}

fn remote_branch_tip(branch_ref: &str, cwd: &Path) -> Result<Option<String>, String> {
    let output = command_output(
        "git",
        &["ls-remote", "--exit-code", "--heads", "origin", branch_ref],
        cwd,
    )?;
    if output.status.code() == Some(2) {
        return Ok(None);
    }
    let output = require_success("git ls-remote", output)?;
    let text = String::from_utf8_lossy(&output);
    let mut fields = text.split_whitespace();
    let oid = fields
        .next()
        .ok_or_else(|| format!("origin returned no object for {branch_ref}"))?;
    let found = fields
        .next()
        .ok_or_else(|| format!("origin returned no ref for {branch_ref}"))?;
    if found != branch_ref {
        return Err(format!(
            "origin returned {found} while querying {branch_ref}"
        ));
    }
    Ok(Some(oid.to_string()))
}

fn merge_tree(head: &str, base: &str, cwd: &Path) -> Result<String, String> {
    let output = command_output(
        "git",
        &[
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            base,
            head,
        ],
        cwd,
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        return stdout
            .lines()
            .next()
            .filter(|tree| !tree.is_empty())
            .map(str::to_string)
            .ok_or_else(|| "git merge-tree returned no tree".to_string());
    }
    if output.status.code() == Some(1) {
        let paths = stdout.lines().skip(1).collect::<Vec<_>>().join(", ");
        return Err(format!(
            "conversation conflicts with the latest {base}: {}",
            if paths.is_empty() {
                "unknown paths"
            } else {
                &paths
            }
        ));
    }
    require_success("git merge-tree", output).map(|_| unreachable!())
}

/// Remove reserved conversation state from the clean publish snapshot.
fn without_caos(tree: &str, cwd: &Path) -> Result<String, String> {
    let listing = capture_required("git", &["ls-tree", tree], cwd)?;
    let retained = listing
        .lines()
        .filter(|line| !line.ends_with("\t.caos"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut child = Command::new("git")
        .arg("mktree")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("running git mktree: {error}"))?;
    if !retained.is_empty() {
        writeln!(child.stdin.as_mut().expect("piped stdin"), "{retained}")
            .map_err(|error| format!("writing git mktree input: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("waiting for git mktree: {error}"))?;
    String::from_utf8(require_success("git mktree", output)?)
        .map(|value| value.trim().to_string())
        .map_err(|error| format!("git mktree returned non-UTF-8 output: {error}"))
}

fn capture_required(program: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    let output = command_output(program, args, cwd)?;
    String::from_utf8(require_success(program, output)?)
        .map(|value| value.trim().to_string())
        .map_err(|error| format!("{program} returned non-UTF-8 output: {error}"))
}

fn command_output(program: &str, args: &[&str], cwd: &Path) -> Result<Output, String> {
    Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("running {program}: {error}"))
}

fn require_success(program: &str, output: Output) -> Result<Vec<u8>, String> {
    if output.status.success() {
        return Ok(output.stdout);
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if detail.is_empty() {
        Err(format!("{program} exited with {}", output.status))
    } else {
        Err(detail)
    }
}

fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(label: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "caos-v2-publish-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        capture_required("git", &["init", "--quiet"], &dir).unwrap();
        capture_required("git", &["config", "user.name", "test"], &dir).unwrap();
        capture_required("git", &["config", "user.email", "test@caos"], &dir).unwrap();
        dir
    }

    #[test]
    fn publish_tree_merges_workspace_and_removes_reserved_state() {
        let dir = repo("clean-tree");
        std::fs::write(dir.join("base.txt"), "base\n").unwrap();
        capture_required("git", &["add", "base.txt"], &dir).unwrap();
        capture_required("git", &["commit", "--quiet", "-m", "base"], &dir).unwrap();
        let base = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();

        std::fs::create_dir(dir.join(".caos")).unwrap();
        std::fs::write(dir.join(".caos/internal"), "private\n").unwrap();
        std::fs::write(dir.join("feature.txt"), "feature\n").unwrap();
        capture_required("git", &["add", "."], &dir).unwrap();
        capture_required("git", &["commit", "--quiet", "-m", "conversation"], &dir).unwrap();
        let head = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();

        let merged = merge_tree(&head, &base, &dir).unwrap();
        let clean = without_caos(&merged, &dir).unwrap();
        assert_eq!(
            capture_required("git", &["show", &format!("{clean}:feature.txt")], &dir).unwrap(),
            "feature"
        );
        assert!(
            !command_output("git", &["cat-file", "-e", &format!("{clean}:.caos")], &dir)
                .unwrap()
                .status
                .success()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
