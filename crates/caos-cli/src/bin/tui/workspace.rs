//! Local-checkout and publication policy for the conversation TUI.

use std::path::Path;
use std::process::{Command, Output};

const REPO_AGENT_PATH: &str = ".caos/agent.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreparedPublishConversation {
    pub(crate) head: String,
}

/// Return whether the freshly fetched PR base is already in the conversation.
///
/// `git merge-base --is-ancestor` reserves exit status 1 for the ordinary
/// "not an ancestor" result. Other failures still need to stop publication.
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PublishedConversationSource {
    pub(crate) id: String,
    pub(crate) title: Option<String>,
    pub(crate) head: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RepoAgentConfig {
    pub(crate) pr_publish_instructions: Option<String>,
}

/// Read the checked-in repository agent configuration.
///
/// Read the blob from Git rather than following the working-tree path: this is
/// explicitly a versioned policy file, and a committed symlink must not be able
/// to copy an arbitrary host file into an agent request.
pub(crate) fn load_repo_agent_config(cwd: &Path) -> Result<RepoAgentConfig, String> {
    let entry = capture_required(
        "git",
        &[
            "ls-tree",
            "--format=%(objectmode) %(objecttype) %(objectname) %(path)",
            "HEAD",
            "--",
            REPO_AGENT_PATH,
        ],
        cwd,
    )?;
    if entry.is_empty() {
        return Ok(RepoAgentConfig::default());
    }
    let fields = entry.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 4
        || !matches!(fields[0], "100644" | "100755")
        || fields[1] != "blob"
        || fields[3] != REPO_AGENT_PATH
    {
        return Err(format!(
            "{REPO_AGENT_PATH} must be a checked-in regular file"
        ));
    }
    let contents = capture_required("git", &["cat-file", "blob", fields[2]], cwd)?;
    let value: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|error| format!("parsing {REPO_AGENT_PATH}: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| format!("{REPO_AGENT_PATH} must contain a JSON object"))?;
    let unknown = object
        .keys()
        .filter(|key| key.as_str() != "pr_publish_instructions")
        .cloned()
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        return Err(format!(
            "{REPO_AGENT_PATH} contains unknown field(s): {}",
            unknown.join(", ")
        ));
    }
    let pr_publish_instructions = match object.get("pr_publish_instructions") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(instructions)) => {
            let instructions = instructions.trim();
            (!instructions.is_empty()).then(|| instructions.to_string())
        }
        Some(_) => {
            return Err(format!(
                "{REPO_AGENT_PATH} field `pr_publish_instructions` must be a string"
            ))
        }
    };
    Ok(RepoAgentConfig {
        pr_publish_instructions,
    })
}

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

/// Commit the current working tree onto the local `HEAD` and return that commit.
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
    capture_required("git", &["rev-parse", "HEAD^{commit}"], cwd)
}

pub(crate) fn prepare_publish_workspace(
    head: &str,
    target: &str,
    cwd: &Path,
) -> Result<PreparedPublishConversation, String> {
    let ancestry = command_output("git", &["merge-base", "--is-ancestor", target, head], cwd)?;
    if !ancestry.status.success() {
        return Err(format!(
            "the publish turn did not merge selected base {}",
            short_hash(target)
        ));
    }

    let conflicts = capture_required(
        "git",
        &["ls-tree", "--format=%(objectname)", head, ".caos/conflicts"],
        cwd,
    )?;
    if !conflicts.is_empty() {
        let conflicts = capture_required("git", &["cat-file", "blob", &conflicts], cwd)?;
        if !conflicts.trim().is_empty() {
            return Err(format!(
                "unresolved merge entries:\n{}",
                conflicts.trim_end()
            ));
        }
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

    let reserved = capture_required(
        "git",
        &["ls-tree", "-r", "--name-only", head, "--", ".caos"],
        cwd,
    )?
    .lines()
    .filter(|path| *path != REPO_AGENT_PATH)
    .map(str::to_string)
    .collect::<Vec<_>>();
    if !reserved.is_empty() {
        return Err(format!(
            "the publish head still contains reserved .caos state: {}",
            reserved.join(", ")
        ));
    }

    Ok(PreparedPublishConversation {
        head: head.to_string(),
    })
}

/// Publish the validated conversation history without checking it out.
pub(crate) fn publish_conversation_pr(
    name: &str,
    title: &str,
    conversation: &PreparedPublishConversation,
    pr_base: &str,
    cwd: &Path,
) -> Result<String, String> {
    let title = conversation_pr_title(title);
    let branch = publish_conversation_branch(name, conversation, cwd)?;

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
        capture_required("gh", &["pr", "edit", &existing_url, "--title", &title], cwd)?;
        return Ok(existing_url);
    }
    let body = format!(
        "Published from virtual CAOS conversation `{name}` at `{}`.",
        short_hash(&conversation.head)
    );
    capture_required(
        "gh",
        &[
            "pr", "create", "--head", &branch, "--base", pr_base, "--title", &title, "--body",
            &body,
        ],
        cwd,
    )
}

/// Push a complete conversation branch without opening or updating a PR.
pub(crate) fn publish_conversation_branch(
    name: &str,
    conversation: &PreparedPublishConversation,
    cwd: &Path,
) -> Result<String, String> {
    let branch = prepare_publish_branch(name, conversation, cwd)?;
    push_publish_branch(&branch, cwd)?;
    Ok(branch)
}

/// Fetch a full conversation branch from an ordinary Git remote or GitHub PR.
///
/// Branch inputs have the form `<remote>/caos/<conversation-id>` (with
/// `caos/<conversation-id>` using `origin`). A GitHub PR is resolved through
/// `gh` so its head branch supplies the stable conversation ID, then fetched
/// through GitHub's pull ref so PRs from forks work without adding a remote.
pub(crate) fn fetch_published_conversation(
    source: &str,
    cwd: &Path,
) -> Result<PublishedConversationSource, String> {
    let source = source.trim();
    if source.is_empty() {
        return Err("published conversation source cannot be empty".to_string());
    }

    if let Some(pull) = parse_github_pull_url(source)? {
        let output = capture_required(
            "gh",
            &["pr", "view", source, "--json", "headRefName,title"],
            cwd,
        )?;
        let metadata: serde_json::Value = serde_json::from_str(&output)
            .map_err(|error| format!("reading GitHub PR metadata: {error}"))?;
        let branch = metadata
            .get("headRefName")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "GitHub PR metadata has no head branch".to_string())?;
        let id = conversation_id_from_branch(branch)?;
        let pr_title = metadata
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&id);
        let title = pr_title
            .strip_prefix("caos conversation: ")
            .unwrap_or(pr_title)
            .to_string();
        let remote = format!("https://github.com/{}/{}.git", pull.owner, pull.repository);
        let remote_ref = format!("refs/pull/{}/head", pull.number);
        let head = fetch_ref_tip(&remote, &remote_ref, cwd)?;
        return Ok(PublishedConversationSource {
            id,
            title: Some(title),
            head,
        });
    }

    let (remote, branch) = parse_remote_conversation_branch(source, cwd)?;
    let id = conversation_id_from_branch(&branch)?;
    // Fetch by URL rather than configured remote name. A named-remote fetch can
    // also apply its default fetch refspec and move unrelated tracking refs;
    // loading one conversation should touch only its temporary import ref.
    let remote_url = capture_required("git", &["remote", "get-url", &remote], cwd)?;
    let head = fetch_ref_tip(&remote_url, &format!("refs/heads/{branch}"), cwd)?;
    Ok(PublishedConversationSource {
        title: None,
        id,
        head,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GithubPull<'a> {
    owner: &'a str,
    repository: &'a str,
    number: &'a str,
}

fn parse_github_pull_url(source: &str) -> Result<Option<GithubPull<'_>>, String> {
    let Some(path) = source
        .strip_prefix("https://github.com/")
        .or_else(|| source.strip_prefix("http://github.com/"))
    else {
        return Ok(None);
    };
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let parts = path.trim_end_matches('/').split('/').collect::<Vec<_>>();
    if parts.len() < 4 || parts[2] != "pull" {
        return Err(format!(
            "unsupported GitHub URL {source:?}; expected https://github.com/<owner>/<repo>/pull/<number>"
        ));
    }
    if parts[0].is_empty()
        || parts[1].is_empty()
        || parts[3].is_empty()
        || !parts[3].bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("invalid GitHub pull request URL {source:?}"));
    }
    Ok(Some(GithubPull {
        owner: parts[0],
        repository: parts[1],
        number: parts[3],
    }))
}

fn parse_remote_conversation_branch(source: &str, cwd: &Path) -> Result<(String, String), String> {
    if source.starts_with("caos/") {
        return Ok(("origin".to_string(), source.to_string()));
    }
    let remotes = capture_required("git", &["remote"], cwd)?;
    let mut remotes = remotes.lines().collect::<Vec<_>>();
    remotes.sort_by_key(|remote| std::cmp::Reverse(remote.len()));
    for remote in remotes {
        let prefix = format!("{remote}/");
        if let Some(branch) = source.strip_prefix(&prefix) {
            if !branch.starts_with("caos/") {
                return Err(format!(
                    "remote branch {source:?} is not a published `caos/<conversation-id>` branch"
                ));
            }
            return Ok((remote.to_string(), branch.to_string()));
        }
    }
    Err(format!(
        "cannot resolve {source:?}; use a GitHub PR URL or <remote>/caos/<conversation-id>"
    ))
}

fn conversation_id_from_branch(branch: &str) -> Result<String, String> {
    let id = branch.strip_prefix("caos/").unwrap_or_default();
    if id.is_empty() {
        return Err(format!(
            "branch {branch:?} does not name a conversation; expected caos/<conversation-id>"
        ));
    }
    Ok(id.to_string())
}

fn fetch_ref_tip(remote: &str, remote_ref: &str, cwd: &Path) -> Result<String, String> {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("reading the clock: {error}"))?
        .as_nanos();
    let temporary_ref = format!("refs/caos/loads/{}-{unique}", std::process::id());
    let refspec = format!("+{remote_ref}:{temporary_ref}");
    let fetched = capture_required(
        "git",
        &["fetch", "--quiet", "--no-tags", remote, &refspec],
        cwd,
    );
    let result = fetched.and_then(|_| {
        capture_required(
            "git",
            &[
                "rev-parse",
                "--verify",
                &format!("{temporary_ref}^{{commit}}"),
            ],
            cwd,
        )
    });
    let cleanup = match result.as_deref() {
        Ok(head) => capture_required("git", &["update-ref", "-d", &temporary_ref, head], cwd),
        Err(_) => capture_required("git", &["update-ref", "-d", &temporary_ref], cwd),
    };
    match (result, cleanup) {
        (Ok(head), Ok(_)) => Ok(head),
        (Ok(_), Err(error)) => Err(format!("removing temporary fetch ref: {error}")),
        (Err(error), _) => Err(error),
    }
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

pub(crate) fn fetch_remote_branch_tip(branch: &str, cwd: &Path) -> Result<String, String> {
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
    let push_ref = format!("{branch_ref}:{branch_ref}");
    let force_migration = match remote_tip.as_deref() {
        None => false,
        Some(remote_tip) => {
            let present = command_output(
                "git",
                &["cat-file", "-e", &format!("{remote_tip}^{{commit}}")],
                cwd,
            )?
            .status
            .success();
            if !present {
                true
            } else {
                let ancestry = command_output(
                    "git",
                    &["merge-base", "--is-ancestor", remote_tip, &branch_ref],
                    cwd,
                )?;
                if ancestry.status.success() {
                    false
                } else if ancestry.status.code() == Some(1) {
                    true
                } else {
                    require_success("git merge-base", ancestry)?;
                    unreachable!("successful merge-base returned through an error branch");
                }
            }
        }
    };
    let lease = force_migration.then(|| {
        format!(
            "--force-with-lease={branch_ref}:{}",
            remote_tip.as_deref().expect("a migration has a remote tip")
        )
    });
    let mut args = vec!["push", "--set-upstream"];
    if let Some(lease) = lease.as_deref() {
        args.push(lease);
    }
    args.extend(["origin", &push_ref]);
    capture_required("git", &args, cwd)?;
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
    conversation: &PreparedPublishConversation,
    cwd: &Path,
) -> Result<String, String> {
    let branch = format!("caos/{name}");
    let branch_ref = format!("refs/heads/{branch}");
    let previous = capture_optional("git", &["rev-parse", "--verify", &branch_ref], cwd)?;
    let publish_commit = &conversation.head;
    match previous.as_deref() {
        Some(old) if old != publish_commit => {
            capture_required(
                "git",
                &["update-ref", &branch_ref, publish_commit, old],
                cwd,
            )?;
        }
        None => {
            capture_required("git", &["update-ref", &branch_ref, publish_commit], cwd)?;
        }
        _ => {}
    }
    Ok(branch)
}

pub(crate) fn capture_required(program: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    capture_required_bytes(program, args, cwd)
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
}

fn capture_required_bytes(program: &str, args: &[&str], cwd: &Path) -> Result<Vec<u8>, String> {
    require_success(program, command_output(program, args, cwd)?)
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

fn conversation_pr_title(title: &str) -> String {
    format!("caos conversation: {title}")
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

    #[test]
    fn published_pr_title_uses_the_conversation_title() {
        assert_eq!(
            conversation_pr_title("Simplify README wording"),
            "caos conversation: Simplify README wording"
        );
    }

    #[test]
    fn github_pull_urls_expose_the_repository_and_pull_number() {
        let parsed = parse_github_pull_url("https://github.com/Metta-AI/caos/pull/34")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.owner, "Metta-AI");
        assert_eq!(parsed.repository, "caos");
        assert_eq!(parsed.number, "34");
        assert!(parse_github_pull_url("https://github.com/Metta-AI/caos/pull/nope").is_err());
        assert_eq!(parse_github_pull_url("origin/caos/talk-1").unwrap(), None);
    }

    #[test]
    fn checked_in_repo_agent_config_loads_pr_publish_instructions() {
        let dir = temp_repo("agent-config");
        commit_file(&dir, "base\n", "base");
        std::fs::create_dir_all(dir.join(".caos")).unwrap();
        std::fs::write(
            dir.join(REPO_AGENT_PATH),
            r#"{"pr_publish_instructions":"Use the repository publication checklist."}"#,
        )
        .unwrap();
        capture_required("git", &["add", REPO_AGENT_PATH], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "config"], &dir).unwrap();

        assert_eq!(
            load_repo_agent_config(&dir).unwrap(),
            RepoAgentConfig {
                pr_publish_instructions: Some(
                    "Use the repository publication checklist.".to_string()
                )
            }
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn repo_agent_config_must_be_a_regular_json_blob_with_known_fields() {
        let dir = temp_repo("agent-symlink");
        commit_file(&dir, "base\n", "base");
        std::fs::create_dir_all(dir.join(".caos")).unwrap();
        std::os::unix::fs::symlink("../file.txt", dir.join(REPO_AGENT_PATH)).unwrap();
        capture_required("git", &["add", REPO_AGENT_PATH], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "symlink"], &dir).unwrap();

        let error = load_repo_agent_config(&dir).unwrap_err();
        assert!(error.contains("checked-in regular file"), "{error}");

        capture_required("git", &["reset", "--hard", "HEAD^"], &dir).unwrap();
        std::fs::create_dir_all(dir.join(".caos")).unwrap();
        std::fs::write(dir.join(REPO_AGENT_PATH), r#"{"publish":true}"#).unwrap();
        capture_required("git", &["add", REPO_AGENT_PATH], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "unknown field"], &dir).unwrap();
        let error = load_repo_agent_config(&dir).unwrap_err();
        assert!(error.contains("unknown field(s): publish"), "{error}");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn repo_agent_config_rejects_invalid_json_and_non_string_instructions() {
        let dir = temp_repo("agent-invalid-json");
        commit_file(&dir, "base\n", "base");
        std::fs::create_dir_all(dir.join(".caos")).unwrap();
        std::fs::write(dir.join(REPO_AGENT_PATH), "not json").unwrap();
        capture_required("git", &["add", REPO_AGENT_PATH], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "invalid json"], &dir).unwrap();
        let error = load_repo_agent_config(&dir).unwrap_err();
        assert!(error.contains("parsing .caos/agent.json"), "{error}");

        std::fs::write(
            dir.join(REPO_AGENT_PATH),
            r#"{"pr_publish_instructions":true}"#,
        )
        .unwrap();
        capture_required("git", &["commit", "-qam", "invalid field"], &dir).unwrap();
        let error = load_repo_agent_config(&dir).unwrap_err();
        assert!(error.contains("must be a string"), "{error}");

        std::fs::remove_dir_all(dir).unwrap();
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
    fn update_tree_commits_the_working_tree_and_returns_its_commit() {
        let dir = temp_repo("snapshot-test");
        let _base = commit_file(&dir, "base\n", "base");
        let head = commit_file(&dir, "head\n", "turn");

        // With a clean checkout nothing is committed and the head commit is
        // returned unchanged.
        let head_tree =
            capture_required("git", &["rev-parse", &format!("{head}^{{tree}}")], &dir).unwrap();
        assert_eq!(commit_working_tree("noop", &dir).unwrap(), head);
        assert_eq!(
            capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap(),
            head
        );

        // Edit a tracked file and add an untracked one, leaving both
        // uncommitted.
        std::fs::write(dir.join("file.txt"), "local edit\n").unwrap();
        std::fs::write(dir.join("new.txt"), "added\n").unwrap();

        let proposal = commit_working_tree("fold in my edits", &dir).unwrap();
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

    #[test]
    fn prepared_publish_head_is_merged_and_has_no_reserved_tip_state() {
        let dir = temp_repo("publish-clean");
        let base = commit_file(&dir, "base\n", "base");
        std::fs::create_dir_all(dir.join(".caos")).unwrap();
        std::fs::write(dir.join(".caos/conflicts"), "").unwrap();
        capture_required("git", &["add", ".caos/conflicts"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "merge resolved"], &dir).unwrap();
        let resolved = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();

        std::fs::remove_dir_all(dir.join(".caos")).unwrap();
        std::fs::write(dir.join("publish.txt"), "ready\n").unwrap();
        capture_required("git", &["add", "-A"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "ready"], &dir).unwrap();
        let head = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();

        let prepared = prepare_publish_workspace(&head, &base, &dir).unwrap();

        assert_eq!(prepared.head, head);
        assert_eq!(
            capture_required(
                "git",
                &["show", &format!("{}:publish.txt", prepared.head)],
                &dir
            )
            .unwrap(),
            "ready"
        );
        assert!(command_output(
            "git",
            &["merge-base", "--is-ancestor", &resolved, &prepared.head],
            &dir
        )
        .unwrap()
        .status
        .success());
        assert!(capture_optional(
            "git",
            &["rev-parse", "--verify", &format!("{}:.caos", prepared.head)],
            &dir
        )
        .unwrap()
        .is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn detects_when_the_remote_base_is_already_in_the_conversation() {
        let dir = temp_repo("base-ancestry");
        let base = commit_file(&dir, "base\n", "base");
        let head = commit_file(&dir, "conversation\n", "conversation");

        assert!(remote_base_is_ancestor(&base, &head, &dir).unwrap());
        assert!(!remote_base_is_ancestor(&head, &base, &dir).unwrap());
        assert!(remote_base_is_ancestor("not-a-commit", &head, &dir).is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prepared_publish_head_allows_repo_agent_instructions() {
        let dir = temp_repo("publish-agent-instructions");
        let base = commit_file(&dir, "base\n", "base");
        std::fs::create_dir_all(dir.join(".caos")).unwrap();
        std::fs::write(
            dir.join(REPO_AGENT_PATH),
            r#"{"pr_publish_instructions":"Build before publishing."}"#,
        )
        .unwrap();
        std::fs::write(dir.join("publish.txt"), "ready\n").unwrap();
        capture_required("git", &["add", REPO_AGENT_PATH, "publish.txt"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "ready"], &dir).unwrap();
        let head = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();

        let prepared = prepare_publish_workspace(&head, &base, &dir).unwrap();

        assert_eq!(
            capture_required(
                "git",
                &["show", &format!("{}:{REPO_AGENT_PATH}", prepared.head)],
                &dir
            )
            .unwrap(),
            r#"{"pr_publish_instructions":"Build before publishing."}"#
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prepared_publish_head_refuses_empty_reserved_tip_state() {
        let dir = temp_repo("publish-reserved");
        let base = commit_file(&dir, "base\n", "base");
        std::fs::create_dir_all(dir.join(".caos")).unwrap();
        std::fs::write(dir.join(".caos/conflicts"), "").unwrap();
        capture_required("git", &["add", ".caos/conflicts"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "resolved"], &dir).unwrap();
        let head = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();

        let error = prepare_publish_workspace(&head, &base, &dir).unwrap_err();

        assert!(error.contains("reserved .caos state"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prepared_publish_head_refuses_unresolved_conflicts() {
        let dir = temp_repo("publish-conflicts");
        let base = commit_file(&dir, "base\n", "base");
        std::fs::create_dir_all(dir.join(".caos")).unwrap();
        std::fs::write(dir.join(".caos/conflicts"), "100644 deadbeef 1\tfile.txt\n").unwrap();
        capture_required("git", &["add", ".caos/conflicts"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "unresolved"], &dir).unwrap();
        let head = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();

        let error = prepare_publish_workspace(&head, &base, &dir).unwrap_err();

        assert!(error.contains("unresolved merge entries"));
        assert!(error.contains("file.txt"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prepared_publish_head_refuses_conflict_markers() {
        let dir = temp_repo("publish-markers");
        let base = commit_file(&dir, "base\n", "base");
        std::fs::write(
            dir.join("workspace"),
            "<<<<<<< ours\nleft\n=======\nright\n>>>>>>> theirs\n",
        )
        .unwrap();
        capture_required("git", &["add", "workspace"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "markers"], &dir).unwrap();
        let head = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();

        let error = prepare_publish_workspace(&head, &base, &dir).unwrap_err();

        assert!(error.contains("unresolved merge markers"));
        assert!(error.contains("workspace"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn publish_branch_preserves_conversation_history_and_repushes_fast_forward() {
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
        std::fs::write(dir.join("inherited.txt"), "from conversation A\n").unwrap();
        capture_required("git", &["add", "inherited.txt"], &dir).unwrap();
        let tool_event = r#"{"author":"assistant","calls":[{"name":"edit"}]}"#;
        let first_head = commit_file(&dir, "first result\n", tool_event);
        capture_required(
            "git",
            &[
                "merge",
                "--quiet",
                "--no-ff",
                "-m",
                "publish merge",
                &main_tip,
            ],
            &dir,
        )
        .unwrap();
        let first_publish = capture_required("git", &["rev-parse", "HEAD"], &dir).unwrap();
        let before = std::fs::read_to_string(dir.join("file.txt")).unwrap();
        let first_conversation =
            prepare_publish_workspace(&first_publish, &main_tip, &dir).unwrap();

        let branch =
            publish_conversation_branch("publish-test", &first_conversation, &dir).unwrap();
        assert_eq!(branch, "caos/publish-test");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            before
        );
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/publish-test"], &dir).unwrap(),
            first_publish
        );
        let branch_ref = "refs/heads/caos/publish-test";
        assert_eq!(
            remote_branch_tip(branch_ref, &dir).unwrap().as_deref(),
            Some(first_publish.as_str())
        );

        let final_head = commit_file(&dir, "final result\n", "terminal assistant event");
        let final_conversation = prepare_publish_workspace(&final_head, &main_tip, &dir).unwrap();
        prepare_publish_branch("publish-test", &final_conversation, &dir).unwrap();
        let final_publish =
            capture_required("git", &["rev-parse", "caos/publish-test"], &dir).unwrap();
        assert_eq!(
            final_publish, final_head,
            "the publish branch must point at the conversation head"
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
            "3"
        );
        assert!(capture_required(
            "git",
            &[
                "log",
                "--format=%s",
                &format!("{main_tip}..caos/publish-test")
            ],
            &dir
        )
        .unwrap()
        .lines()
        .any(|message| message == tool_event));
        assert!(command_output(
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
        assert!(command_output(
            "git",
            &["merge-base", "--is-ancestor", &first_head, &final_publish],
            &dir
        )
        .unwrap()
        .status
        .success());

        // The remote still has the previous conversation head. Pushing the new
        // head advances it without rewriting the published conversation.
        assert_eq!(
            remote_branch_tip(branch_ref, &dir).unwrap().as_deref(),
            Some(first_publish.as_str())
        );
        push_publish_branch(&branch, &dir).unwrap();
        assert_eq!(
            remote_branch_tip(branch_ref, &dir).unwrap().as_deref(),
            Some(final_publish.as_str())
        );

        // An auto-deleted merged branch is recreated by an ordinary create.
        capture_required("git", &[&git_dir, "update-ref", "-d", branch_ref], &dir).unwrap();
        assert_eq!(remote_branch_tip(branch_ref, &dir).unwrap(), None);
        push_publish_branch(&branch, &dir).unwrap();
        assert_eq!(
            remote_branch_tip(branch_ref, &dir).unwrap().as_deref(),
            Some(final_publish.as_str())
        );

        prepare_publish_branch("publish-test", &final_conversation, &dir).unwrap();
        assert_eq!(
            capture_required("git", &["rev-parse", "caos/publish-test"], &dir).unwrap(),
            final_publish
        );

        // A branch made by the old publisher is a one-commit snapshot and is
        // not an ancestor of the conversation. Migrate that shape once under
        // an exact lease; subsequent updates take the fast-forward path above.
        let final_tree = capture_required("git", &["rev-parse", "HEAD^{tree}"], &dir).unwrap();
        let legacy_snapshot = capture_required(
            "git",
            &[
                "commit-tree",
                &final_tree,
                "-p",
                &main_tip,
                "-m",
                "legacy CAOS snapshot",
            ],
            &dir,
        )
        .unwrap();
        capture_required(
            "git",
            &["update-ref", branch_ref, &legacy_snapshot, &final_publish],
            &dir,
        )
        .unwrap();
        let legacy_push = format!("+{branch_ref}:{branch_ref}");
        capture_required("git", &["push", "--quiet", "origin", &legacy_push], &dir).unwrap();
        prepare_publish_branch("publish-test", &final_conversation, &dir).unwrap();
        push_publish_branch(&branch, &dir).unwrap();
        assert_eq!(
            remote_branch_tip(branch_ref, &dir).unwrap().as_deref(),
            Some(final_publish.as_str())
        );

        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(remote).unwrap();
    }

    #[test]
    fn publish_uses_the_conversation_head_merged_with_the_selected_base() {
        let dir = temp_repo("publish-stacked-test");
        let base = commit_file(&dir, "base\n", "base");
        let _parent_head = commit_file(&dir, "parent conversation\n", "parent turn");
        let child_head = commit_file(&dir, "child conversation\n", "child turn");

        capture_required("git", &["switch", "--detach", "-q", &base], &dir).unwrap();
        std::fs::write(dir.join("upstream.txt"), "upstream\n").unwrap();
        capture_required("git", &["add", "upstream.txt"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "base advances"], &dir).unwrap();
        let selected_base = commit_file(&dir, "parent conversation\n", "parent conversation head");

        std::fs::write(dir.join("file.txt"), "child conversation\n").unwrap();
        capture_required("git", &["add", "file.txt"], &dir).unwrap();
        capture_required("git", &["commit", "-q", "-m", "assembled tree"], &dir).unwrap();
        let tree = capture_required("git", &["rev-parse", "HEAD^{tree}"], &dir).unwrap();
        let merged_head = capture_required(
            "git",
            &[
                "commit-tree",
                &tree,
                "-p",
                &child_head,
                "-p",
                &selected_base,
                "-m",
                "publish merge event",
            ],
            &dir,
        )
        .unwrap();
        let conversation = prepare_publish_workspace(&merged_head, &selected_base, &dir).unwrap();
        prepare_publish_branch("stacked-test", &conversation, &dir).unwrap();

        assert_eq!(
            capture_required("git", &["rev-parse", "caos/stacked-test"], &dir).unwrap(),
            merged_head
        );
        assert_eq!(
            capture_required("git", &["show", "caos/stacked-test:file.txt"], &dir).unwrap(),
            "child conversation"
        );
        assert_eq!(
            capture_required("git", &["show", "caos/stacked-test:upstream.txt"], &dir).unwrap(),
            "upstream"
        );
        assert!(command_output(
            "git",
            &["merge-base", "--is-ancestor", &selected_base, &merged_head],
            &dir
        )
        .unwrap()
        .status
        .success());
        assert!(command_output(
            "git",
            &["merge-base", "--is-ancestor", &child_head, &merged_head],
            &dir
        )
        .unwrap()
        .status
        .success());

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

    #[test]
    fn remote_conversation_branches_fetch_without_moving_tracking_refs() {
        let dir = temp_repo("load-remote-branch");
        let remote = dir.with_extension("remote.git");
        let remote_path = remote.to_string_lossy().to_string();
        capture_required("git", &["init", "--bare", "-q", &remote_path], &dir).unwrap();
        capture_required("git", &["remote", "add", "origin", &remote_path], &dir).unwrap();
        let head = commit_file(&dir, "published conversation\n", "conversation event");
        capture_required(
            "git",
            &[
                "push",
                "--quiet",
                "origin",
                &format!("{head}:refs/heads/caos/shared/talk-1"),
            ],
            &dir,
        )
        .unwrap();
        capture_required(
            "git",
            &["update-ref", "-d", "refs/remotes/origin/caos/shared/talk-1"],
            &dir,
        )
        .unwrap();

        let loaded = fetch_published_conversation("origin/caos/shared/talk-1", &dir).unwrap();
        assert_eq!(loaded.id, "shared/talk-1");
        assert_eq!(loaded.title, None);
        assert_eq!(loaded.head, head);
        assert!(capture_optional(
            "git",
            &[
                "rev-parse",
                "--verify",
                "refs/remotes/origin/caos/shared/talk-1"
            ],
            &dir,
        )
        .unwrap()
        .is_none());
        assert!(capture_required(
            "git",
            &["for-each-ref", "--format=%(refname)", "refs/caos/loads"],
            &dir,
        )
        .unwrap()
        .is_empty());

        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(remote).unwrap();
    }
}
