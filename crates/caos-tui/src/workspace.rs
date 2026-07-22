use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use caos::chat::WorkspaceDiff;

/// Apply a conversation's accumulated workspace change to a clean checkout.
///
/// This is deliberately client policy rather than part of the chat engine:
/// the TUI chooses when to mutate the checkout and requires confirmation before
/// calling it.
pub(crate) fn load_conversation_workspace(diff: &WorkspaceDiff, cwd: &Path) -> Result<(), String> {
    let dirty = capture_required(
        "git",
        &["status", "--porcelain=v1", "--untracked-files=all"],
        cwd,
    )?;
    if !dirty.is_empty() {
        return Err(
            "the working tree is not clean; commit or stash local changes before applying the conversation workspace"
                .to_string(),
        );
    }
    let patch = capture_required_raw(
        "git",
        &[
            "diff",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-color",
            &diff.base,
            &diff.head,
        ],
        cwd,
    )?;
    if patch.is_empty() {
        return Ok(());
    }
    git_apply(&patch, true, cwd)?;
    git_apply(&patch, false, cwd)
}

/// Publish the virtual workspace as a clean branch without checking it out.
///
/// Conversation commits retain their internal step DAG as second parents. A
/// PR should not expose that implementation history, so the publish branch is
/// a clean sequence of snapshot commits whose trees match conversation heads.
pub(crate) fn publish_conversation_pr(name: &str, diff: &WorkspaceDiff) -> Result<String, String> {
    let cwd = Path::new(".");
    let branch = prepare_publish_branch(name, diff, cwd)?;
    let branch_ref = format!("refs/heads/{branch}");
    let push_ref = format!("{branch_ref}:refs/heads/{branch}");
    capture_required("git", &["push", "--set-upstream", "origin", &push_ref], cwd)?;

    if let Some(url) = capture_optional(
        "gh",
        &["pr", "view", &branch, "--json", "url", "--jq", ".url"],
        cwd,
    )?
    .filter(|url| !url.is_empty())
    {
        return Ok(url);
    }
    let body = format!(
        "Published from virtual CAOS conversation `{name}` at `{}`.\n\nThe working tree was not modified.",
        short_hash(&diff.head)
    );
    capture_required(
        "gh",
        &[
            "pr",
            "create",
            "--head",
            &branch,
            "--title",
            &format!("CAOS conversation {name}"),
            "--body",
            &body,
        ],
        cwd,
    )
}

pub(crate) fn prepare_publish_branch(
    name: &str,
    diff: &WorkspaceDiff,
    cwd: &Path,
) -> Result<String, String> {
    let branch = format!("caos/{name}");
    let branch_ref = format!("refs/heads/{branch}");
    let head_tree_spec = format!("{}^{{tree}}", diff.head);
    let head_tree = capture_required("git", &["rev-parse", &head_tree_spec], cwd)?;
    let previous = capture_optional("git", &["rev-parse", "--verify", &branch_ref], cwd)?;
    let publish_commit = if let Some(previous) = previous.as_deref() {
        let previous_tree_spec = format!("{previous}^{{tree}}");
        let previous_tree = capture_required("git", &["rev-parse", &previous_tree_spec], cwd)?;
        if previous_tree == head_tree {
            previous.to_string()
        } else {
            capture_required(
                "git",
                &[
                    "commit-tree",
                    &head_tree,
                    "-p",
                    previous,
                    "-m",
                    &format!("Update CAOS conversation {name}"),
                ],
                cwd,
            )?
        }
    } else {
        capture_required(
            "git",
            &[
                "commit-tree",
                &head_tree,
                "-p",
                &diff.base,
                "-m",
                &format!("CAOS conversation {name}"),
            ],
            cwd,
        )?
    };
    match previous.as_deref() {
        Some(old) if old != publish_commit => {
            capture_required(
                "git",
                &["update-ref", &branch_ref, &publish_commit, old],
                cwd,
            )?;
        }
        None => {
            capture_required("git", &["update-ref", &branch_ref, &publish_commit], cwd)?;
        }
        _ => {}
    }
    Ok(branch)
}

pub(crate) fn capture_required(program: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    let output = command_output(program, args, cwd)?;
    require_success(program, output).map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
}

fn capture_required_raw(program: &str, args: &[&str], cwd: &Path) -> Result<Vec<u8>, String> {
    let output = command_output(program, args, cwd)?;
    require_success(program, output)
}

fn capture_optional(program: &str, args: &[&str], cwd: &Path) -> Result<Option<String>, String> {
    let output = command_output(program, args, cwd)?;
    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
    } else {
        Ok(None)
    }
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
    Err(if detail.is_empty() {
        format!("{program} exited with {}", output.status)
    } else {
        detail
    })
}

fn git_apply(patch: &[u8], check: bool, cwd: &Path) -> Result<(), String> {
    let mut command = Command::new("git");
    command.arg("apply");
    if check {
        command.arg("--check");
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .current_dir(cwd);
    let mut child = command
        .spawn()
        .map_err(|error| format!("running git apply: {error}"))?;
    child
        .stdin
        .take()
        .ok_or("git apply stdin was not piped")?
        .write_all(patch)
        .map_err(|error| format!("writing patch to git apply: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("waiting for git apply: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let action = if check { "checking" } else { "applying" };
    Err(format!(
        "{action} the conversation workspace failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn short_hash(hash: &str) -> &str {
    hash.get(..7).unwrap_or(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(label: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("caos-tui-{label}-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        capture_required("git", &["init", "-q"], &dir).unwrap();
        capture_required("git", &["config", "user.name", "Test User"], &dir).unwrap();
        capture_required("git", &["config", "user.email", "test@example.com"], &dir).unwrap();
        dir
    }

    fn commit_file(dir: &Path, content: &str, message: &str) -> String {
        std::fs::write(dir.join("file.txt"), content).unwrap();
        capture_required("git", &["add", "file.txt"], dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", message], dir).unwrap();
        capture_required("git", &["rev-parse", "HEAD"], dir).unwrap()
    }

    #[test]
    fn load_requires_a_clean_checkout_and_applies_the_conversation_diff() {
        let dir = temp_repo("load-test");
        let base = commit_file(&dir, "base\n", "base");
        let head = commit_file(&dir, "conversation result\n", "turn");
        capture_required("git", &["switch", "--detach", "-q", &base], &dir).unwrap();
        let diff = WorkspaceDiff {
            base,
            head,
            stat: String::new(),
            patch: "changed".to_string(),
        };

        load_conversation_workspace(&diff, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "conversation result\n"
        );
        assert!(load_conversation_workspace(&diff, &dir)
            .unwrap_err()
            .contains("working tree is not clean"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn publish_branch_is_a_clean_snapshot_without_checkout_changes() {
        let dir = temp_repo("publish-test");
        let base = commit_file(&dir, "base\n", "base");
        let head = commit_file(&dir, "conversation result\n", "internal turn");
        let before = std::fs::read_to_string(dir.join("file.txt")).unwrap();
        let diff = WorkspaceDiff {
            base: base.clone(),
            head: head.clone(),
            stat: String::new(),
            patch: "changed".to_string(),
        };

        let branch = prepare_publish_branch("publish-test", &diff, &dir).unwrap();
        assert_eq!(branch, "caos/publish-test");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            before
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/publish-test^{tree}"], &dir).unwrap(),
            capture_required("git", &["rev-parse", &format!("{head}^{{tree}}")], &dir).unwrap()
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/publish-test^"], &dir).unwrap(),
            base
        );
        let first = capture_required("git", &["rev-parse", "caos/publish-test"], &dir).unwrap();
        prepare_publish_branch("publish-test", &diff, &dir).unwrap();
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/publish-test"], &dir).unwrap(),
            first
        );

        std::fs::remove_dir_all(dir).unwrap();
    }
}
