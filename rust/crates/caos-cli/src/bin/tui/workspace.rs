//! Local-checkout and PR publication policy for the conversation TUI.

use std::path::Path;
use std::process::{Command, Output};

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

/// Commit the current working tree onto the local `HEAD` and return the new
/// commit together with its shared ancestor with the selected workspace.
///
/// This is the inverse of `load_conversation_workspace`: after checking out a
/// conversation head and editing files, `/update-tree` folds those files into a
/// user-authored turn. It deliberately DOES commit — staging everything with
/// `git add -A` and committing when the tree is dirty — so the checkout is left
/// clean and its `HEAD` matches exactly what the turn receives. A later
/// `Ctrl+L` onto the conversation's new head then succeeds instead of tripping
/// the clean-tree guard. When the working tree is already clean (the user
/// committed the changes themselves), nothing is committed and the current
/// `HEAD` is returned. `git add -A` respects `.gitignore`, so the commit
/// mirrors what a normal commit of the working tree would contain.
pub(crate) fn commit_working_tree(
    message: &str,
    workspace: &str,
    cwd: &Path,
) -> Result<(String, String), String> {
    // HEAD can already contain user commits. Their delta starts at the shared
    // ancestor, not at HEAD just before staging the remaining edits.
    let base = capture_required("git", &["merge-base", "--all", workspace, "HEAD"], cwd).map_err(
        |error| format!("cannot find a shared base for the checkout and workspace: {error}"),
    )?;
    if base.lines().count() != 1 {
        return Err(
            "checkout and workspace have multiple merge bases; merge them before /update-tree"
                .to_string(),
        );
    }
    capture_required("git", &["add", "-A"], cwd)?;
    // `git diff --cached --quiet` exits non-zero exactly when the index differs
    // from HEAD, i.e. there is something to commit.
    let clean = command_output("git", &["diff", "--cached", "--quiet"], cwd)?
        .status
        .success();
    if !clean {
        capture_required("git", &["commit", "--quiet", "-m", message], cwd)?;
    }
    let proposal = capture_required("git", &["rev-parse", "HEAD^{commit}"], cwd)?;
    Ok((proposal, base))
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

pub(crate) fn remote_base_is_ancestor(
    target: &str,
    head: &str,
    cwd: &Path,
) -> Result<bool, String> {
    let ancestry = command_output("git", &["merge-base", "--is-ancestor", target, head], cwd)?;
    match ancestry.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => {
            require_success("git merge-base --is-ancestor", ancestry)?;
            unreachable!("a successful command has exit status 0")
        }
    }
}

/// Accept either a branch name or the familiar origin/<branch> spelling.
pub(crate) fn pr_base_branch(input: &str) -> &str {
    input.trim().strip_prefix("origin/").unwrap_or(input.trim())
}

pub(crate) fn validate_prepared_workspace(
    target: &str,
    head: &str,
    cwd: &Path,
) -> Result<(), String> {
    if !remote_base_is_ancestor(target, head, cwd)? {
        return Err("the preparation turn did not merge the selected PR base".to_string());
    }
    let markers = command_output(
        "git",
        &[
            "grep",
            "-I",
            "-n",
            "-e",
            "^<<<<<<< ",
            "-e",
            "^=======$",
            "-e",
            "^>>>>>>> ",
            head,
            "--",
        ],
        cwd,
    )?;
    if markers.status.success() {
        return Err(format!(
            "unresolved merge markers:\n{}",
            String::from_utf8_lossy(&markers.stdout).trim_end()
        ));
    }
    if markers.status.code() != Some(1) {
        require_success("git grep", markers)?;
    }
    Ok(())
}

pub(crate) fn find_or_open_workspace_pr_in(
    repository: &str,
    name: &str,
    title: &str,
    published: &caos_cli::PublishedBranch,
    base: &str,
    cwd: &Path,
) -> Result<String, String> {
    let repository = caos_cli::normalize_repository_identity(repository)?;
    find_or_open_workspace_pr_with(&repository, name, title, published, base, |args| {
        capture_required("gh", args, cwd)
    })
}

pub(crate) fn lookup_workspace_pr(
    repository: &str,
    branch: &str,
    cwd: &Path,
) -> Result<Option<String>, String> {
    let repository = caos_cli::normalize_repository_identity(repository)?;
    lookup_workspace_pr_with(&repository, branch, &mut |args| {
        capture_required("gh", args, cwd)
    })
}

fn lookup_workspace_pr_with(
    repository: &str,
    branch: &str,
    gh: &mut impl FnMut(&[&str]) -> Result<String, String>,
) -> Result<Option<String>, String> {
    let matches = gh(&[
        "pr", "list", "--repo", repository, "--head", branch, "--state", "open", "--json", "url",
        "--jq", ".[].url",
    ])?;
    let urls = matches
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    match urls.as_slice() {
        [] => Ok(None),
        [url] => Ok(Some((*url).to_string())),
        _ => Err(format!(
            "several open PRs use branch {branch:?}; choose a distinct publication branch"
        )),
    }
}

fn find_or_open_workspace_pr_with(
    repository: &str,
    name: &str,
    title: &str,
    published: &caos_cli::PublishedBranch,
    base: &str,
    mut gh: impl FnMut(&[&str]) -> Result<String, String>,
) -> Result<String, String> {
    if let Some(existing) = lookup_workspace_pr_with(repository, &published.branch, &mut gh)? {
        gh(&[
            "pr", "edit", &existing, "--repo", repository, "--base", base,
        ])?;
        return Ok(existing);
    }
    let body = format!(
        "Published workspace `{}` from CAOS conversation `{name}` at `{}`.",
        published.workspace, published.head
    );
    gh(&[
        "pr",
        "create",
        "--repo",
        repository,
        "--head",
        &published.branch,
        "--base",
        base,
        "--title",
        title,
        "--body",
        &body,
    ])
}

pub(crate) fn capture_required(program: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
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
    fn pr_preparation_fetches_the_base_without_touching_local_edits() {
        let remote = temp_repo("pr-origin");
        capture_required("git", &["branch", "-M", "release/next"], &remote).unwrap();
        let base = commit_file(&remote, "base\n", "base");
        let repo = temp_repo("pr-local");
        let local = commit_file(&repo, "local\n", "local");
        capture_required(
            "git",
            &["remote", "add", "origin", remote.to_str().unwrap()],
            &repo,
        )
        .unwrap();
        std::fs::write(repo.join("file.txt"), "staged\n").unwrap();
        capture_required("git", &["add", "file.txt"], &repo).unwrap();
        std::fs::write(repo.join("file.txt"), "unstaged\n").unwrap();
        let index = capture_required("git", &["write-tree"], &repo).unwrap();

        let transport = caos::GitTransport::discover(&repo).unwrap();
        let remote_url = remote.to_str().unwrap();
        assert_eq!(
            caos_cli::workspaces::default_branch(&transport, remote_url).unwrap(),
            "release/next"
        );
        assert_eq!(pr_base_branch(" origin/release/next "), "release/next");
        assert_eq!(pr_base_branch("release/next"), "release/next");
        assert_eq!(
            caos_cli::workspaces::branch_snapshot(&transport, remote_url, "release/next").unwrap(),
            base
        );
        assert!(
            caos_cli::workspaces::branch_snapshot(&transport, remote_url, "missing")
                .unwrap_err()
                .contains("does not exist")
        );
        assert!(
            caos_cli::workspaces::branch_snapshot(&transport, remote_url, "../invalid").is_err()
        );
        validate_prepared_workspace(&base, &base, &repo).unwrap();
        assert!(validate_prepared_workspace(&base, &local, &repo)
            .unwrap_err()
            .contains("did not merge"));
        assert!(validate_prepared_workspace("bad-revision", &base, &repo).is_err());
        let conflicted = commit_file(
            &remote,
            "<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\n",
            "conflict",
        );
        assert_eq!(
            caos_cli::workspaces::branch_snapshot(&transport, remote_url, "release/next").unwrap(),
            conflicted
        );
        assert!(validate_prepared_workspace(&base, &conflicted, &repo)
            .unwrap_err()
            .contains("unresolved merge markers"));
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD"], &repo).unwrap(),
            local
        );
        assert_eq!(
            capture_required("git", &["write-tree"], &repo).unwrap(),
            index
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("file.txt")).unwrap(),
            "unstaged\n"
        );
        std::fs::remove_dir_all(repo).unwrap();
        std::fs::remove_dir_all(remote).unwrap();
    }

    #[test]
    fn pr_publication_creates_or_reuses_the_matching_origin_pr() {
        let published = caos_cli::PublishedBranch {
            workspace: "docs".to_string(),
            branch: "caos/talk-1".to_string(),
            head: "a".repeat(40),
            publication: "publication-1".to_string(),
            status: conversation_protocol::v3::PublicationStatus::Complete,
            observed: Some("a".repeat(40)),
        };
        let url = "https://github.com/owner/repo/pull/1";
        for existing in [false, true] {
            let mut calls = Vec::new();
            let result = find_or_open_workspace_pr_with(
                "https://github.com/owner/repo",
                "talk-1",
                "Fix documentation",
                &published,
                "release/next",
                |args| {
                    calls.push(args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
                    Ok(if args[1] == "list" && !existing {
                        ""
                    } else {
                        url
                    }
                    .to_string())
                },
            )
            .unwrap();
            assert_eq!(result, url);
            assert_eq!(calls.len(), 2);
            assert_eq!(
                calls[0],
                [
                    "pr",
                    "list",
                    "--repo",
                    "https://github.com/owner/repo",
                    "--head",
                    "caos/talk-1",
                    "--state",
                    "open",
                    "--json",
                    "url",
                    "--jq",
                    ".[].url"
                ]
            );
            assert_eq!(calls[1][1], if existing { "edit" } else { "create" });
            if existing {
                assert!(
                    !calls[1].iter().any(|arg| arg == "--title"),
                    "preserve a manually edited PR title"
                );
                assert!(calls[1]
                    .windows(2)
                    .any(|pair| pair == ["--base", "release/next"]));
            } else {
                assert!(calls[1]
                    .windows(2)
                    .any(|pair| pair == ["--title", "Fix documentation"]));
            }
            assert!(calls[1]
                .windows(2)
                .any(|pair| pair == ["--repo", "https://github.com/owner/repo"]));
            if existing {
                assert_eq!(calls[1][2], url);
            } else {
                assert!(calls[1]
                    .windows(2)
                    .any(|pair| pair == ["--head", "caos/talk-1"]));
                assert!(calls[1]
                    .windows(2)
                    .any(|pair| pair == ["--base", "release/next"]));
                assert!(!calls[1].iter().any(|arg| arg == "--draft"));
            }
        }
        let mut calls = 0;
        let error = find_or_open_workspace_pr_with(
            "owner/repo",
            "talk-1",
            "title",
            &published,
            "main",
            |_| {
                calls += 1;
                Err("GitHub unavailable".to_string())
            },
        )
        .unwrap_err();
        assert_eq!(error, "GitHub unavailable");
        assert_eq!(
            calls, 1,
            "a failed lookup must not attempt to create another PR"
        );
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
    fn update_tree_includes_committed_edits_and_uses_the_shared_workspace_base() {
        let dir = temp_repo("committed-update");
        let base = commit_file(&dir, "base\n", "base");
        let local = commit_file(&dir, "committed edit\n", "local");
        capture_required("git", &["checkout", "--detach", &base], &dir).unwrap();
        std::fs::write(dir.join("remote.txt"), "remote edit\n").unwrap();
        capture_required("git", &["add", "remote.txt"], &dir).unwrap();
        capture_required("git", &["commit", "-qm", "remote"], &dir).unwrap();
        let remote = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();
        capture_required("git", &["checkout", "--detach", &local], &dir).unwrap();

        assert_eq!(
            commit_working_tree("use committed edit", &remote, &dir).unwrap(),
            (local.clone(), base.clone())
        );
        std::fs::write(dir.join("new.txt"), "uncommitted edit\n").unwrap();
        let (proposal, proposal_base) = commit_working_tree("include both", &remote, &dir).unwrap();
        assert_eq!(proposal_base, base);
        assert_eq!(
            capture_required("git", &["rev-parse", &format!("{proposal}^")], &dir).unwrap(),
            local
        );
        assert_eq!(
            capture_required("git", &["show", &format!("{proposal}:file.txt")], &dir).unwrap(),
            "committed edit"
        );
        assert_eq!(
            capture_required("git", &["show", &format!("{proposal}:new.txt")], &dir).unwrap(),
            "uncommitted edit"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unrelated_workspace_is_rejected_before_staging_local_edits() {
        let dir = temp_repo("unrelated-update");
        let head = commit_file(&dir, "base\n", "base");
        let tree = capture_required("git", &["rev-parse", "HEAD^{tree}"], &dir).unwrap();
        let unrelated =
            capture_required("git", &["commit-tree", &tree, "-m", "unrelated"], &dir).unwrap();
        std::fs::write(dir.join("file.txt"), "dirty\n").unwrap();
        let error = commit_working_tree("update", &unrelated, &dir).unwrap_err();
        assert!(error.contains("shared base"), "{error}");
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap(),
            head
        );
        capture_required("git", &["diff", "--cached", "--quiet"], &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "dirty\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn update_tree_commits_the_working_tree_and_returns_its_commit() {
        let dir = temp_repo("snapshot-test");
        let _base = commit_file(&dir, "base\n", "base");
        let head = commit_file(&dir, "head\n", "turn");

        // With a clean checkout nothing is committed and the head commit is
        // returned unchanged.
        let head_tree =
            capture_required("git", &["rev-parse", &format!("{head}^{{tree}}")], &dir).unwrap();
        assert_eq!(
            commit_working_tree("noop", &head, &dir).unwrap(),
            (head.clone(), head.clone())
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap(),
            head
        );

        // Edit a tracked file and add an untracked one, leaving both
        // uncommitted.
        std::fs::write(dir.join("file.txt"), "local edit\n").unwrap();
        std::fs::write(dir.join("new.txt"), "added\n").unwrap();

        let (proposal, proposal_base) =
            commit_working_tree("fold in my edits", &head, &dir).unwrap();
        assert_eq!(proposal_base, head);
        let tree =
            capture_required("git", &["rev-parse", &format!("{proposal}^{{tree}}")], &dir).unwrap();

        // The returned commit carries exactly the working tree and its base.
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
}
