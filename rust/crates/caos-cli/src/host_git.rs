//! Host Git operations shared by clients.

use std::path::Path;
use std::process::{Command, Output};

/// Check out a conversation's head commit in the local working tree.
///
/// This is deliberately client policy rather than part of the chat engine:
/// the TUI requires an explicit local destination, which it can remember.
/// Rather than applying the base-to-head diff as unstaged changes,
/// this moves the local HEAD onto the conversation head commit so the checkout
/// exactly matches it.
pub fn load_conversation_source_tree(head: &str, cwd: &Path) -> Result<(), String> {
    let dirty = capture_required(
        "git",
        &["status", "--porcelain=v1", "--untracked-files=all"],
        cwd,
    )?;
    if !dirty.is_empty() {
        return Err(
            "the working tree is not clean; commit or stash local changes before checking out the conversation head"
                .to_string(),
        );
    }
    capture_required("git", &["checkout", "--detach", head], cwd)?;
    Ok(())
}

/// Resolve the default branch and its tip from the LOCAL branch, without
/// touching the network.
///
/// Starting a new conversation only needs a base commit to build on, and the
/// tip of your local default branch (e.g. `main`) is a fine one. This discovers
/// the default branch *name* from the `origin/HEAD` symref, then reads the local
/// `refs/heads/<name>` — not the `origin/<name>` tracking ref — so it reflects
/// your checked-out branch as it is right now. It runs no `git ls-remote`/`git
/// fetch`, so it stays instant (e.g. on every Ctrl+N) instead of blocking on
/// round-trips to `origin`.
pub fn local_default_branch_tip(cwd: &Path) -> Result<(String, String), String> {
    // `refs/remotes/origin/HEAD` is the local symref recording origin's default
    // branch; it is set at clone time and refreshed by `git remote set-head`.
    let head_ref = capture_required("git", &["symbolic-ref", "refs/remotes/origin/HEAD"], cwd)
        .map_err(|error| {
            format!(
                "could not resolve origin's default branch locally \
             (run `git remote set-head origin -a`): {error}"
            )
        })?;
    let branch = head_ref
        .strip_prefix("refs/remotes/origin/")
        .ok_or_else(|| format!("origin/HEAD points outside refs/remotes/origin: {head_ref}"))?
        .to_string();
    let local_ref = format!("refs/heads/{branch}");
    let commit = capture_required("git", &["rev-parse", "--verify", &local_ref], cwd)
        .map_err(|error| format!("local default branch {branch:?} not found: {error}"))?;
    Ok((branch, commit))
}

pub fn capture_required(program: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    capture_required_bytes(program, args, cwd)
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
}

fn capture_required_bytes(program: &str, args: &[&str], cwd: &Path) -> Result<Vec<u8>, String> {
    require_success(program, command_output(program, args, cwd)?)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(label: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "caos-cli-tui-{label}-{}-{unique}",
            std::process::id()
        ));
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
    fn load_requires_a_clean_checkout_and_checks_out_the_conversation_head() {
        let dir = temp_repo("load-test");
        let base = commit_file(&dir, "base\n", "base");
        let head = commit_file(&dir, "conversation result\n", "turn");
        capture_required("git", &["switch", "--detach", "-q", &base], &dir).unwrap();
        load_conversation_source_tree(&head, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "conversation result\n"
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap(),
            head
        );

        std::fs::write(dir.join("file.txt"), "local edit\n").unwrap();
        assert!(load_conversation_source_tree(&head, &dir)
            .unwrap_err()
            .contains("working tree is not clean"));

        std::fs::remove_dir_all(dir).unwrap();
    }
}
