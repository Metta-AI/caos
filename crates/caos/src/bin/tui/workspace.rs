use std::path::Path;
use std::process::{Command, Output};

use caos::chat::WorkspaceDiff;

/// Check out a conversation's head commit in the local working tree.
///
/// This is deliberately client policy rather than part of the chat engine:
/// the TUI chooses when to mutate the checkout and requires confirmation before
/// calling it. Rather than applying the base-to-head diff as unstaged changes,
/// this moves the local HEAD onto the conversation head commit so the checkout
/// exactly matches it.
pub(crate) fn load_conversation_workspace(head: &str, cwd: &Path) -> Result<(), String> {
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

/// Publish the virtual workspace as a clean branch without checking it out.
///
/// Conversation commits retain their internal step DAG as second parents. A
/// PR should not expose that implementation history or retain superseded
/// snapshots, so the publish branch is one clean commit above the freshly
/// fetched PR-base tip. Publishing to the default branch merges the complete
/// workspace; publishing elsewhere replays this conversation's delta so a
/// child can target its parent's clean snapshot without duplicating that work.
pub(crate) fn publish_conversation_pr(
    name: &str,
    diff: &WorkspaceDiff,
    pr_base: &str,
    default_base: &str,
) -> Result<String, String> {
    let cwd = Path::new(".");
    let branch = format!("caos/{name}");
    let pr_base_commit = fetch_remote_branch_tip(pr_base, cwd)?;
    let change_base = (pr_base != default_base).then_some(diff.base_commit.as_str());
    prepare_publish_branch(name, diff, &pr_base_commit, change_base, cwd)?;
    push_publish_branch(&branch, cwd)?;

    let existing_url = capture_required(
        "gh",
        &[
            "pr",
            "list",
            "--head",
            &branch,
            "--base",
            pr_base,
            "--state",
            "open",
            "--json",
            "url",
            "--jq",
            ".[0].url // empty",
        ],
        cwd,
    )?;
    if !existing_url.is_empty() {
        return Ok(existing_url);
    }
    let body = format!(
        "Published from virtual CAOS conversation `{name}` at `{}`.",
        short_hash(&diff.head)
    );
    capture_required(
        "gh",
        &[
            "pr",
            "create",
            "--head",
            &branch,
            "--base",
            pr_base,
            "--title",
            &format!("CAOS conversation {name}"),
            "--body",
            &body,
        ],
        cwd,
    )
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
/// round-trips to `origin`. Publishing a PR still fetches, where a fresh remote
/// tip matters.
pub(crate) fn local_default_branch_tip(cwd: &Path) -> Result<(String, String), String> {
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

pub(crate) fn remote_default_branch(cwd: &Path) -> Result<String, String> {
    let output = command_output("git", &["ls-remote", "--symref", "origin", "HEAD"], cwd)?;
    let stdout = require_success("git", output)?;
    parse_remote_default_branch(&String::from_utf8_lossy(&stdout))
}

fn parse_remote_default_branch(output: &str) -> Result<String, String> {
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let (Some(marker), Some(reference), Some(target)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if marker == "ref:" && target == "HEAD" {
            let branch = reference
                .strip_prefix("refs/heads/")
                .ok_or_else(|| format!("origin HEAD points outside refs/heads: {reference}"))?;
            if branch.is_empty() {
                return Err("origin HEAD advertises an empty default branch".to_string());
            }
            return Ok(branch.to_string());
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

fn push_publish_branch(branch: &str, cwd: &Path) -> Result<(), String> {
    let branch_ref = format!("refs/heads/{branch}");
    let remote_tip = remote_branch_tip(&branch_ref, cwd)?;
    let expected = remote_tip.as_deref().unwrap_or("");
    let lease = format!("--force-with-lease={branch_ref}:{expected}");
    let push_ref = format!("{branch_ref}:{branch_ref}");
    capture_required(
        "git",
        &["push", "--set-upstream", &lease, "origin", &push_ref],
        cwd,
    )?;
    Ok(())
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
    let stdout = require_success("git", output)?;
    let result = String::from_utf8_lossy(&stdout);
    let mut fields = result.split_whitespace();
    let oid = fields
        .next()
        .ok_or_else(|| format!("git ls-remote returned no object for {branch_ref}"))?;
    let found_ref = fields
        .next()
        .ok_or_else(|| format!("git ls-remote returned no ref for {branch_ref}"))?;
    if found_ref != branch_ref {
        return Err(format!(
            "git ls-remote returned {found_ref} while querying {branch_ref}"
        ));
    }
    Ok(Some(oid.to_string()))
}

pub(crate) fn prepare_publish_branch(
    name: &str,
    diff: &WorkspaceDiff,
    publish_base: &str,
    change_base: Option<&str>,
    cwd: &Path,
) -> Result<String, String> {
    let branch = format!("caos/{name}");
    let branch_ref = format!("refs/heads/{branch}");
    let publish_tree = merge_publish_tree(&diff.head, publish_base, change_base, cwd)?;
    let previous = capture_optional("git", &["rev-parse", "--verify", &branch_ref], cwd)?;
    let reusable = if let Some(previous) = previous.as_deref() {
        let previous_tree_spec = format!("{previous}^{{tree}}");
        let previous_tree = capture_required("git", &["rev-parse", &previous_tree_spec], cwd)?;
        let previous_parent =
            capture_optional("git", &["rev-parse", &format!("{previous}^")], cwd)?;
        previous_tree == publish_tree && previous_parent.as_deref() == Some(publish_base)
    } else {
        false
    };
    let publish_commit = if reusable {
        previous.clone().expect("a reusable publish commit exists")
    } else {
        capture_required(
            "git",
            &[
                "commit-tree",
                &publish_tree,
                "-p",
                publish_base,
                "-m",
                &format!(
                    "CAOS conversation {} at {}",
                    short_hash(name),
                    short_hash(&diff.head)
                ),
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

/// Merge the conversation's final state with the current PR base without
/// touching the real index or working tree. With no `change_base`, Git chooses
/// the natural merge base so a default-branch publish includes inherited
/// filesystem work. With one, only changes after that commit are replayed onto
/// a non-default PR base. Only the resulting tree is retained either way.
fn merge_publish_tree(
    head: &str,
    publish_base: &str,
    change_base: Option<&str>,
    cwd: &Path,
) -> Result<String, String> {
    // A child conversation starts from its parent's internal commit, while a
    // stacked PR targets the parent's clean snapshot commit. Those commits
    // intentionally have different histories. Give the selected snapshot a
    // temporary parent at the conversation's starting point so merge-tree
    // applies only this conversation's delta instead of re-merging its parent.
    let merge_tip = if let Some(change_base) = change_base {
        let publish_tree = capture_required(
            "git",
            &["rev-parse", &format!("{publish_base}^{{tree}}")],
            cwd,
        )?;
        capture_required(
            "git",
            &[
                "commit-tree",
                &publish_tree,
                "-p",
                change_base,
                "-m",
                "temporary CAOS publish merge base",
            ],
            cwd,
        )?
    } else {
        publish_base.to_string()
    };
    let output = command_output(
        "git",
        &[
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            &merge_tip,
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
            .ok_or_else(|| "git merge-tree returned no merged tree".to_string());
    }
    if output.status.code() == Some(1) {
        let conflicts = stdout
            .lines()
            .skip(1)
            .filter(|path| !path.is_empty())
            .map(|path| format!("  {path}"))
            .collect::<Vec<_>>();
        let paths = if conflicts.is_empty() {
            "  (git did not report the conflicting paths)".to_string()
        } else {
            conflicts.join("\n")
        };
        return Err(format!(
            "conversation changes conflict with the selected PR base branch:\n{paths}\nresolve these files against the latest selected base before publishing"
        ));
    }
    require_success("git merge-tree", output).map(|_| unreachable!())
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
        load_conversation_workspace(&head, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "conversation result\n"
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap(),
            head
        );

        std::fs::write(dir.join("file.txt"), "local edit\n").unwrap();
        assert!(load_conversation_workspace(&head, &dir)
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
    fn publish_branch_is_one_replaceable_snapshot_without_checkout_changes() {
        let dir = temp_repo("publish-test");
        let remote = dir.with_extension("remote.git");
        let remote_path = remote.to_string_lossy().to_string();
        capture_required("git", &["init", "--bare", "-q", &remote_path], &dir).unwrap();
        capture_required("git", &["remote", "add", "origin", &remote_path], &dir).unwrap();
        let base = commit_file(&dir, "base\n", "base");
        std::fs::write(dir.join("main.txt"), "new on main\n").unwrap();
        capture_required("git", &["add", "main.txt"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "main advances"], &dir).unwrap();
        let main_tip = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();
        let git_dir = format!("--git-dir={remote_path}");
        capture_required(
            "git",
            &[&git_dir, "symbolic-ref", "HEAD", "refs/heads/main"],
            &dir,
        )
        .unwrap();
        let default_push = format!("{main_tip}:refs/heads/main");
        capture_required("git", &["push", "--quiet", "origin", &default_push], &dir).unwrap();
        assert_eq!(remote_default_branch(&dir).unwrap(), "main");
        assert_eq!(fetch_remote_branch_tip("main", &dir).unwrap(), main_tip);

        capture_required("git", &["switch", "--detach", "-q", &base], &dir).unwrap();
        std::fs::write(dir.join("key.txt"), "temporary secret\n").unwrap();
        std::fs::write(dir.join("inherited.txt"), "from conversation A\n").unwrap();
        capture_required("git", &["add", "key.txt", "inherited.txt"], &dir).unwrap();
        let first_head = commit_file(&dir, "first result\n", "internal turn with key");
        let before = std::fs::read_to_string(dir.join("file.txt")).unwrap();
        let first_diff = WorkspaceDiff {
            base_commit: base.clone(),
            head: first_head.clone(),
            patch: "changed".to_string(),
        };

        let branch =
            prepare_publish_branch("publish-test", &first_diff, &main_tip, None, &dir).unwrap();
        assert_eq!(branch, "caos/publish-test");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            before
        );
        let first_publish =
            capture_required("git", &["rev-parse", "caos/publish-test"], &dir).unwrap();
        push_publish_branch(&branch, &dir).unwrap();
        let branch_ref = "refs/heads/caos/publish-test";
        assert_eq!(
            remote_branch_tip(branch_ref, &dir).unwrap().as_deref(),
            Some(first_publish.as_str())
        );

        capture_required("git", &["rm", "-q", "key.txt"], &dir).unwrap();
        let final_head = commit_file(&dir, "final result\n", "internal turn without key");
        let final_diff = WorkspaceDiff {
            // This conversation was started from the previous conversation's
            // head. Publishing must not make that internal history reachable.
            base_commit: first_head.clone(),
            head: final_head.clone(),
            patch: "changed again".to_string(),
        };
        prepare_publish_branch("publish-test", &final_diff, &main_tip, None, &dir).unwrap();
        let final_publish =
            capture_required("git", &["rev-parse", "caos/publish-test"], &dir).unwrap();
        assert_ne!(final_publish, first_publish);
        assert_eq!(
            capture_required(
                "git",
                &["show", "-s", "--format=%s", "caos/publish-test"],
                &dir
            )
            .unwrap(),
            format!(
                "CAOS conversation {} at {}",
                short_hash("publish-test"),
                short_hash(&final_head)
            )
        );
        assert_eq!(
            capture_required("git", &["show", "caos/publish-test:file.txt"], &dir).unwrap(),
            "final result"
        );
        assert_eq!(
            capture_required("git", &["show", "caos/publish-test:main.txt"], &dir).unwrap(),
            "new on main"
        );
        assert_eq!(
            capture_required("git", &["show", "caos/publish-test:inherited.txt"], &dir).unwrap(),
            "from conversation A"
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/publish-test^"], &dir).unwrap(),
            main_tip
        );
        assert_eq!(
            capture_required(
                "git",
                &[
                    "rev-list",
                    "--count",
                    &format!("{main_tip}..caos/publish-test")
                ],
                &dir
            )
            .unwrap(),
            "1"
        );
        assert!(
            capture_optional("git", &["show", "caos/publish-test:key.txt"], &dir)
                .unwrap()
                .is_none()
        );
        assert!(!command_output(
            "git",
            &[
                "merge-base",
                "--is-ancestor",
                &first_publish,
                &final_publish
            ],
            &dir
        )
        .unwrap()
        .status
        .success());
        assert!(!command_output(
            "git",
            &["merge-base", "--is-ancestor", &first_head, &final_publish],
            &dir
        )
        .unwrap()
        .status
        .success());

        // A failed earlier publish can leave the local snapshot ahead of a
        // different remote tip. Lease against the freshly queried remote tip,
        // not the stale local branch that this call is about to push.
        assert_eq!(
            remote_branch_tip(branch_ref, &dir).unwrap().as_deref(),
            Some(first_publish.as_str())
        );
        push_publish_branch(&branch, &dir).unwrap();
        assert_eq!(
            remote_branch_tip(branch_ref, &dir).unwrap().as_deref(),
            Some(final_publish.as_str())
        );

        // An auto-deleted merged branch is recreated only if it is still
        // absent when the force-with-lease push reaches the remote.
        capture_required("git", &[&git_dir, "update-ref", "-d", branch_ref], &dir).unwrap();
        assert_eq!(remote_branch_tip(branch_ref, &dir).unwrap(), None);
        push_publish_branch(&branch, &dir).unwrap();
        assert_eq!(
            remote_branch_tip(branch_ref, &dir).unwrap().as_deref(),
            Some(final_publish.as_str())
        );

        prepare_publish_branch("publish-test", &final_diff, &main_tip, None, &dir).unwrap();
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/publish-test"], &dir).unwrap(),
            final_publish
        );

        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(remote).unwrap();
    }

    #[test]
    fn publish_refuses_conflicts_without_changing_the_checkout_or_branch() {
        let dir = temp_repo("publish-conflict-test");
        let base = commit_file(&dir, "base\n", "base");
        let conversation_head = commit_file(&dir, "conversation\n", "conversation turn");
        capture_required("git", &["switch", "--detach", "-q", &base], &dir).unwrap();
        let main_tip = commit_file(&dir, "main\n", "main advances");

        let diff = WorkspaceDiff {
            base_commit: base,
            head: conversation_head,
            patch: "changed".to_string(),
        };
        let error =
            prepare_publish_branch("conflict-test", &diff, &main_tip, None, &dir).unwrap_err();

        assert!(error.contains("conflict with the selected PR base branch"));
        assert!(error.contains("file.txt"));
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap(),
            main_tip
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "main\n"
        );
        assert!(capture_optional(
            "git",
            &["rev-parse", "--verify", "refs/heads/caos/conflict-test"],
            &dir
        )
        .unwrap()
        .is_none());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn non_default_publish_replays_only_the_child_conversation_delta() {
        let dir = temp_repo("publish-stacked-test");
        let base = commit_file(&dir, "base\n", "base");
        let parent_head = commit_file(&dir, "parent conversation\n", "parent turn");
        let child_head = commit_file(&dir, "child conversation\n", "child turn");

        capture_required("git", &["switch", "--detach", "-q", &base], &dir).unwrap();
        std::fs::write(dir.join("upstream.txt"), "upstream\n").unwrap();
        capture_required("git", &["add", "upstream.txt"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "base advances"], &dir).unwrap();
        let selected_base = commit_file(
            &dir,
            "parent conversation\n",
            "clean parent conversation snapshot",
        );

        // Merging both complete histories repeats the parent's edit and
        // conflicts with the child's edit to the same line.
        assert!(merge_publish_tree(&child_head, &selected_base, None, &dir).is_err());

        let diff = WorkspaceDiff {
            base_commit: parent_head.clone(),
            head: child_head,
            patch: "changed".to_string(),
        };
        prepare_publish_branch(
            "stacked-test",
            &diff,
            &selected_base,
            Some(&parent_head),
            &dir,
        )
        .unwrap();

        assert_eq!(
            capture_required("git", &["show", "caos/stacked-test:file.txt"], &dir).unwrap(),
            "child conversation"
        );
        assert_eq!(
            capture_required("git", &["show", "caos/stacked-test:upstream.txt"], &dir).unwrap(),
            "upstream"
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/stacked-test^"], &dir).unwrap(),
            selected_base
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remote_default_branch_comes_from_the_origin_head_symref() {
        assert_eq!(
            parse_remote_default_branch(
                "ref: refs/heads/release/next\tHEAD\n0123456789abcdef\tHEAD\n"
            )
            .unwrap(),
            "release/next"
        );
        assert!(parse_remote_default_branch("0123456789abcdef\tHEAD\n")
            .unwrap_err()
            .contains("did not advertise"));
        assert!(parse_remote_default_branch("ref: refs/tags/v1\tHEAD\n")
            .unwrap_err()
            .contains("outside refs/heads"));
    }
}
