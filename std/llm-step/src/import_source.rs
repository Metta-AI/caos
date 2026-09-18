//! Ref resolution is pinned in tool progress before importing any objects.
use super::*;
use std::process::Command;

pub(super) const HELP: &str = "Import an HTTPS Git repository as a new, unchanged snapshot at an unused conversation path. Returns its full commit hash; preserves its tree and ancestor history. Does not merge or overwrite code. Public repositories need no token. For origin/main, select the repository from the chosen source's .source.json provenance and request main; use an explicit repository when ambiguous. Local checkouts use the client's /import command.
@param source HTTPS repository URL, without credentials.
@param [revision] Remote branch, full refs/... name, or full commit hash. Omit for the remote default branch. A new call observes the remote again.
@param into Unused conversation path, such as imports/repo/main-2. Its .source.json sibling must also be free.";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    source: String,
    revision: Option<String>,
    into: String,
}

fn parameters(call: &Call) -> Result<Parameters, String> {
    let args: Parameters = serde_json::from_value(call.input.clone())
        .map_err(|error| format!("invalid import_source arguments: {error}"))?;
    git_locator::import::remote(&args.source)?;
    if let Some(revision) = &args.revision {
        conversation_protocol::v3::source_trees::validate_branch(revision)?;
    }
    paths::validate_source_tree_name(&args.into)?;
    paths::validate_source_tree_name(&format!("{}.source.json", args.into))?;
    Ok(args)
}

// The mounted secret is a GitHub credential, not a credential for every
// HTTPS repository the model can name. Explicit CLI callers choose their token.
fn is_github_remote(source: &str) -> bool {
    git_locator::import::remote(source).is_ok_and(|(host, _)| {
        host.eq_ignore_ascii_case("github.com") || host.eq_ignore_ascii_case("github.com:443")
    })
}

pub(super) fn token_file(source: &str) -> Option<&'static str> {
    (is_github_remote(source) && Path::new("/secret/github-token").exists())
        .then_some("/secret/github-token")
}

fn free_destination(view: &Conversation<'_>, into: &str) -> Result<(), String> {
    for name in [into.to_string(), format!("{into}.source.json")] {
        // entry refuses traversal through a gitlink, file or symlink as well.
        if view.snapshot().exists(&name)? {
            return Err(format!(
                "import destination {name:?} is occupied; choose an unused path"
            ));
        }
    }
    Ok(())
}

pub(super) fn execute(state: &mut progress::State, site: &CallSite<'_>) -> Result<(), String> {
    let args = match parameters(site.call) {
        Ok(p) => p,
        Err(error) => return site.fail(state, &error),
    };
    let Parameters {
        source,
        revision,
        into,
    } = &args;
    let revision = revision.as_deref();
    let token_file = token_file(source);
    if let Err(error) = free_destination(&state.conversation()?, into) {
        return site.fail(state, &error);
    }
    let provenance = match pinned(&state.conversation()?, site)? {
        Some(value) => value,
        None => {
            let token = token_file
                .map(fs::read_to_string)
                .transpose()
                .map_err(|_| "reading GitHub token")?;
            let token = token.as_deref().map(str::trim_end);
            let (commit, default_branch) = match resolve(source, revision, token) {
                Ok(value) => value,
                Err(_) => return site.fail(state, "Remote Git revision could not be resolved. Check the repository, revision and token."),
            };
            let observation = json!({
                "repository":source, "requested_revision":revision, "commit":commit,
                "default_branch":default_branch,
                "observed_at":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| "clock precedes Unix epoch")?.as_secs()
            });
            let Some(value) = pin(state, site, &observation)? else {
                return Ok(());
            };
            value
        }
    };
    let commit = Oid::parse(
        provenance["commit"]
            .as_str()
            .ok_or("import pin lacks commit")?,
        "imported commit",
    )?;
    let mut command = Command::new("caos");
    command.args(["import-git", source, commit.as_str()]);
    if let Some(path) = token_file {
        command.arg(format!("--github-token-file={path}"));
    }
    let output = command
        .output()
        .map_err(|e| format!("starting import-git: {e}"))?;
    if !output.status.success() {
        return site.fail(
            state,
            "Remote Git import failed. Check the repository, commit and token, then retry.",
        );
    }
    if std::str::from_utf8(&output.stdout).map(str::trim).ok() != Some(commit.as_str()) {
        return Err("import-git returned a different commit".into());
    }
    attach(state, site, into, &commit, &provenance)
}

fn resolve(
    source: &str,
    revision: Option<&str>,
    token: Option<&str>,
) -> Result<(String, Option<String>), String> {
    let revision = revision.unwrap_or("HEAD");
    if git_locator::import::commit(revision) {
        return Ok((revision.to_ascii_lowercase(), None));
    }
    let revision = if revision == "HEAD" || revision.starts_with("refs/") {
        revision.to_string()
    } else {
        format!("refs/heads/{revision}")
    };
    let output = git_locator::import::git(source, token)?
        .args([
            "ls-remote",
            "--symref",
            "--",
            source,
            &revision,
            &format!("{revision}^{{}}"),
        ])
        .output()
        .map_err(|_| "starting remote ref lookup")?;
    if !output.status.success() {
        return Err("remote ref lookup failed".into());
    }
    parse_ref(
        &revision,
        std::str::from_utf8(&output.stdout).map_err(|_| "invalid Git response")?,
    )
}

fn parse_ref(revision: &str, output: &str) -> Result<(String, Option<String>), String> {
    let mut hash = None;
    let mut peeled = None;
    let mut default_branch = None;
    for line in output.lines() {
        if let Some((left, right)) = line.split_once('\t') {
            if right == revision {
                if let Some(branch) = left.strip_prefix("ref: refs/heads/") {
                    if revision == "HEAD" {
                        default_branch = Some(branch.to_string());
                    }
                } else if git_locator::import::commit(left) {
                    hash = Some(left.to_ascii_lowercase());
                }
            } else if right == format!("{revision}^{{}}") && git_locator::import::commit(left) {
                peeled = Some(left.to_ascii_lowercase());
            }
        }
    }
    Ok((
        peeled.or(hash).ok_or("remote revision not found")?,
        default_branch,
    ))
}

fn pin_path(site: &CallSite<'_>) -> String {
    format!(
        "{}/import.json",
        paths::call_payload_dir(site.request.as_str(), site.round, &site.call.id)
    )
}

fn pinned(view: &Conversation<'_>, site: &CallSite<'_>) -> Result<Option<Value>, String> {
    match view.tool(site.request, site.round, &site.call.id)? {
        Some(record) if record.status == CallStatus::Started => {
            let bytes = view.payload(&pin_path(site))?;
            Ok(Some(
                serde_json::from_slice(&bytes).map_err(|_| "invalid import pin")?,
            ))
        }
        _ => Ok(None),
    }
}

/// Append the observation before transfer. A competing attempt's saved hash wins.
pub(super) fn pin<S: progress::RefStore>(
    state: &mut progress::State<S>,
    site: &CallSite<'_>,
    observation: &Value,
) -> Result<Option<Value>, String> {
    for _ in 0..32 {
        state.reload()?;
        let view = state.conversation()?;
        if let Some(record) = view.tool(site.request, site.round, &site.call.id)? {
            return if record.is_terminal() {
                Ok(None)
            } else {
                pinned(&view, site)
            };
        }
        let mut record = site.stub(None);
        record.status = CallStatus::Started;
        let expected = state.head().clone();
        state.try_append_at(
            &expected,
            Transition::ToolStart {
                record,
                payloads: vec![("import.json".into(), canonical_payload_bytes(observation)?)],
            },
        )?;
    }
    Err("conversation kept moving while pinning import".into())
}

pub(super) fn attach<S: progress::RefStore>(
    state: &mut progress::State<S>,
    site: &CallSite<'_>,
    into: &str,
    commit: &Oid,
    provenance: &Value,
) -> Result<(), String> {
    for _ in 0..32 {
        state.reload()?;
        if state
            .conversation()?
            .tool(site.request, site.round, &site.call.id)?
            .is_some_and(|r| r.is_terminal())
        {
            return Ok(());
        }
        let stub = site.stub(None);
        let (block, files) = match free_destination(&state.conversation()?, into) {
            Ok(()) => (
                result_block(
                    &site.call.id,
                    &format!("Imported {commit} at {into}"),
                    false,
                ),
                vec![
                    (into.to_string(), Some((Mode::Commit, commit.encode_line()))),
                    (
                        format!("{into}.source.json"),
                        Some((Mode::Blob, canonical_payload_bytes(provenance)?)),
                    ),
                ],
            ),
            Err(error) => (error_block(&site.call.id, &error), Vec::new()),
        };
        let mut record = completed_record(
            &stub,
            ToolResult::Complete {
                observation: observation_path(&stub),
                proposal: None,
            },
            None,
        );
        record.files = files.iter().map(|(name, _)| name.clone()).collect();
        record.files.sort();
        let expected = state.head().clone();
        if matches!(
            state.try_append_at(&expected, tool_complete_transition(record, &block, files)?)?,
            progress::TryAppend::Appended(_)
        ) {
            return Ok(());
        }
    }
    Err("conversation kept moving while attaching import".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_exact_refs_default_branch_and_annotated_tags() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        assert_eq!(
            parse_ref("HEAD", &format!("ref: refs/heads/main\tHEAD\n{a}\tHEAD\n")).unwrap(),
            (a.clone(), Some("main".into()))
        );
        assert_eq!(
            parse_ref(
                "refs/tags/v1",
                &format!("{a}\trefs/tags/v1\n{b}\trefs/tags/v1^{{}}\n")
            )
            .unwrap(),
            (b, None)
        );
        assert!(parse_ref("refs/heads/main", &format!("{a}\trefs/heads/other\n")).is_err());
        assert_eq!(
            resolve("https://host/repo", Some(&a.to_uppercase()), None).unwrap(),
            (a, None)
        );
    }

    #[test]
    fn automatic_github_credentials_stay_on_github() {
        for source in [
            "https://github.com/owner/repo",
            "https://GitHub.com:443/owner/repo",
        ] {
            assert!(is_github_remote(source), "{source}");
        }
        for source in [
            "https://example.com/repo",
            "https://github.com.evil.example/repo",
            "https://github.com:444/repo",
            "https://github.com@evil.example/repo",
            "https://localhost:5003/repo",
            "http://github.com/owner/repo",
        ] {
            assert!(!is_github_remote(source), "{source}");
        }
    }

    #[test]
    fn rejects_bad_arguments_before_transfer() {
        for input in [
            json!({}),
            json!({"source":"/local", "into":"imports/x"}),
            json!({"source":"https://host/repo", "into":".caos/x"}),
            json!({"source":"https://host/repo", "into":"../x"}),
            json!({"source":"https://host/repo", "into":"imports/x", "revision":4}),
            json!({"source":"https://host/repo", "into":"imports/x", "revision":"../main"}),
            json!({"source":"https://host/repo", "into":"imports/x", "revision":"main*"}),
            json!({"source":"https://host/repo", "into":"imports/x", "extra":"x"}),
        ] {
            assert!(parameters(&Call {
                id: "a".into(),
                name: "import_source".into(),
                input
            })
            .is_err());
        }
    }
}
