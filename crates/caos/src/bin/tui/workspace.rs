use std::path::Path;
use std::process::{Command, Output};

use caos::chat::WorkspaceDiff;

/// Check out a conversation's head commit in the local working tree.
///
/// This is deliberately client policy rather than part of the chat engine:
/// the TUI chooses when to mutate the checkout. Rather than applying the
/// base-to-head diff as unstaged changes,
/// this moves the local HEAD onto the conversation head commit so the checkout
/// exactly matches it.
pub(crate) fn load_conversation_workspace(diff: &WorkspaceDiff, cwd: &Path) -> Result<(), String> {
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
    capture_required("git", &["checkout", "--detach", &diff.head], cwd)?;
    Ok(())
}

/// Commit the current working tree onto the local `HEAD` and return the tree
/// hash the commit carries.
///
/// This is the inverse of `load_conversation_workspace`: after checking out a
/// conversation head and editing files, `/update-tree` folds those files into a
/// user-authored turn. It deliberately DOES commit — staging everything with
/// `git add -A` and committing when the tree is dirty — so the checkout is left
/// clean and its `HEAD` matches exactly what the turn receives. A later
/// `Ctrl+L` onto the conversation's new head then succeeds instead of tripping
/// the clean-tree guard. When the working tree is already clean (the user
/// committed the changes themselves), nothing is committed and the current
/// `HEAD`'s tree is returned. `git add -A` respects `.gitignore`, so the commit
/// mirrors what a normal commit of the working tree would contain.
pub(crate) fn commit_working_tree(message: &str, cwd: &Path) -> Result<String, String> {
    capture_required("git", &["add", "-A"], cwd)?;
    // `git diff --cached --quiet` exits non-zero exactly when the index differs
    // from HEAD, i.e. there is something to commit.
    let clean = command_output("git", &["diff", "--cached", "--quiet"], cwd)?
        .status
        .success();
    if !clean {
        capture_required("git", &["commit", "--quiet", "-m", message], cwd)?;
    }
    capture_required("git", &["rev-parse", "HEAD^{tree}"], cwd)
}

/// Merge `target` into the selected virtual conversation and advance its local
/// engine ref. The synthetic human/agent pair keeps the conversation's
/// first-parent transcript structure intact while the human commit records the
/// target as its second parent.
pub(crate) fn sync_conversation(
    conversation: &str,
    target: &str,
    cwd: &Path,
) -> Result<String, String> {
    let conversation_ref = format!("refs/caos/conversations/{conversation}");
    let head = capture_required("git", &["rev-parse", "--verify", &conversation_ref], cwd)?;
    let target_commit = capture_required(
        "git",
        &["rev-parse", "--verify", &format!("{target}^{{commit}}")],
        cwd,
    )?;
    let ancestor = command_output(
        "git",
        &["merge-base", "--is-ancestor", &target_commit, &head],
        cwd,
    )?;
    if ancestor.status.success() {
        return Ok(head);
    }
    if ancestor.status.code() != Some(1) {
        require_success("git", ancestor)?;
    }
    let tree = merge_tree(&head, &target_commit, cwd)?;
    let author = capture_required("git", &["config", "user.name"], cwd)?;
    if author == "caos-agent" {
        return Err(
            "your git author name is \"caos-agent\", which is reserved for agent commits"
                .to_string(),
        );
    }
    let human = capture_required(
        "git",
        &[
            "commit-tree",
            &tree,
            "-p",
            &head,
            "-p",
            &target_commit,
            "-m",
            &format!("/sync {target}"),
        ],
        cwd,
    )?;
    let synced = capture_required(
        "git",
        &[
            "-c",
            "user.name=caos-agent",
            "-c",
            "user.email=caos@caos",
            "commit-tree",
            &tree,
            "-p",
            &human,
            "-m",
            &format!("Synced {target} into the conversation."),
        ],
        cwd,
    )?;
    capture_required(
        "git",
        &["update-ref", &conversation_ref, &synced, &head],
        cwd,
    )?;
    Ok(synced)
}

fn merge_tree(head: &str, target: &str, cwd: &Path) -> Result<String, String> {
    let output = command_output("git", &["merge-tree", "--write-tree", head, target], cwd)?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    let detail = [output.stdout, output.stderr]
        .into_iter()
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Err(format!("sync has merge conflicts:\n{detail}"))
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
        let diff = WorkspaceDiff {
            base,
            head: head.clone(),
            stat: String::new(),
            patch: "changed".to_string(),
        };

        load_conversation_workspace(&diff, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "conversation result\n"
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap(),
            head
        );

        std::fs::write(dir.join("file.txt"), "local edit\n").unwrap();
        assert!(load_conversation_workspace(&diff, &dir)
            .unwrap_err()
            .contains("working tree is not clean"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn update_tree_commits_the_working_tree_and_returns_its_tree() {
        let dir = temp_repo("snapshot-test");
        let _base = commit_file(&dir, "base\n", "base");
        let head = commit_file(&dir, "head\n", "turn");

        // With a clean checkout nothing is committed and the head's tree is
        // returned unchanged.
        let head_tree =
            capture_required("git", &["rev-parse", &format!("{head}^{{tree}}")], &dir).unwrap();
        assert_eq!(commit_working_tree("noop", &dir).unwrap(), head_tree);
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap(),
            head
        );

        // Edit a tracked file and add an untracked one, leaving both
        // uncommitted.
        std::fs::write(dir.join("file.txt"), "local edit\n").unwrap();
        std::fs::write(dir.join("new.txt"), "added\n").unwrap();

        let tree = commit_working_tree("fold in my edits", &dir).unwrap();

        // The returned tree is exactly the working tree.
        assert_ne!(tree, head_tree);
        assert_eq!(
            capture_required("git", &["show", &format!("{tree}:file.txt")], &dir).unwrap(),
            "local edit"
        );
        assert_eq!(
            capture_required("git", &["show", &format!("{tree}:new.txt")], &dir).unwrap(),
            "added"
        );

        // The edits are now committed on HEAD, so the checkout is clean and a
        // later checkout of a new head would succeed.
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD^"], &dir).unwrap(),
            head
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD^{tree}"], &dir).unwrap(),
            tree
        );
        assert!(capture_required("git", &["status", "--porcelain=v1"], &dir)
            .unwrap()
            .is_empty());
        assert_eq!(
            capture_required("git", &["show", "-s", "--format=%s", "HEAD"], &dir).unwrap(),
            "fold in my edits"
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sync_merges_the_target_and_records_a_conversation_turn() {
        let dir = temp_repo("sync-test");
        let base = commit_file(&dir, "base\n", "base");
        capture_required("git", &["switch", "-c", "conversation"], &dir).unwrap();
        std::fs::write(dir.join("conversation.txt"), "conversation\n").unwrap();
        capture_required("git", &["add", "conversation.txt"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "conversation"], &dir).unwrap();
        let conversation_human = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();
        let conversation_tree =
            capture_required("git", &["rev-parse", "HEAD^{tree}"], &dir).unwrap();
        let conversation_head = capture_required(
            "git",
            &[
                "-c",
                "user.name=caos-agent",
                "-c",
                "user.email=caos@caos",
                "commit-tree",
                &conversation_tree,
                "-p",
                &conversation_human,
                "-m",
                "Conversation response.",
            ],
            &dir,
        )
        .unwrap();
        capture_required(
            "git",
            &[
                "update-ref",
                "refs/caos/conversations/talk-1",
                &conversation_head,
            ],
            &dir,
        )
        .unwrap();
        capture_required("git", &["switch", "--detach", &base], &dir).unwrap();
        std::fs::write(dir.join("target.txt"), "target\n").unwrap();
        capture_required("git", &["add", "target.txt"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "target"], &dir).unwrap();
        let target = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();

        let synced = sync_conversation("talk-1", &target, &dir).unwrap();
        assert_eq!(
            capture_required(
                "git",
                &["rev-parse", "refs/caos/conversations/talk-1"],
                &dir
            )
            .unwrap(),
            synced
        );
        assert_eq!(
            capture_required("git", &["show", "-s", "--format=%an", &synced], &dir).unwrap(),
            "caos-agent"
        );
        let human = capture_required("git", &["rev-parse", &format!("{synced}^")], &dir).unwrap();
        assert_eq!(
            capture_required("git", &["rev-parse", &format!("{human}^1")], &dir).unwrap(),
            conversation_head
        );
        assert_eq!(
            capture_required("git", &["rev-parse", &format!("{human}^2")], &dir).unwrap(),
            target
        );
        assert_eq!(
            capture_required(
                "git",
                &["show", &format!("{synced}:conversation.txt")],
                &dir
            )
            .unwrap(),
            "conversation"
        );
        assert_eq!(
            capture_required("git", &["show", &format!("{synced}:target.txt")], &dir).unwrap(),
            "target"
        );
        assert_eq!(sync_conversation("talk-1", &target, &dir).unwrap(), synced);
        let transport = caos::GitTransport::discover(&dir).unwrap();
        assert_eq!(
            caos::chat::conversation_history(&transport, "talk-1")
                .unwrap()
                .into_iter()
                .map(|turn| turn.message)
                .collect::<Vec<_>>(),
            [
                "conversation",
                "Conversation response.",
                &format!("/sync {target}"),
                &format!("Synced {target} into the conversation."),
            ]
        );

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
