//! An inline import has a persisted call identity, but no compute dispatch.
use super::*;
use std::process::Command;

pub(super) const HELP: &str = "Import an HTTPS Git repository as a new, unchanged snapshot at an unused conversation path. Returns its full commit hash; preserves its tree and ancestor history. Does not merge or overwrite code. Public repositories need no token. For origin/main, select the repository from the chosen source's .source.json provenance and request main; use an explicit repository when ambiguous. Local checkouts use the client's /import command.
@param source HTTPS repository URL, without credentials.
@param [revision] Remote branch, full refs/... name, or full commit hash. Omit for the remote default branch. A new call observes the remote again.
@param into Unused conversation path, such as imports/repo/main-2. Its .source.json sibling must also be free.";

fn parameters(call: &Call) -> Result<(&str, Option<&str>, &str), String> {
    let object = call
        .input
        .as_object()
        .ok_or("import_source requires an object")?;
    if object
        .keys()
        .any(|k| !["source", "revision", "into"].contains(&k.as_str()))
    {
        return Err("unknown import_source argument".into());
    }
    let required = |name| {
        object
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("import_source requires {name}"))
    };
    let source = required("source")?;
    let into = required("into")?;
    let revision = object
        .get("revision")
        .map(|v| v.as_str().ok_or("revision must be a string"))
        .transpose()?;
    conversation_protocol::v3::source_trees::validate_remote_import(source, revision)?;
    paths::validate_source_tree_name(into)?;
    paths::validate_source_tree_name(&format!("{into}.source.json"))?;
    Ok((source, revision, into))
}

// The mounted secret is a GitHub credential, not a credential for every
// HTTPS repository the model can name. Explicit CLI callers choose their token.
fn is_github_remote(source: &str) -> bool {
    source
        .strip_prefix("https://")
        .and_then(|url| url.split_once('/'))
        .is_some_and(|(host, _)| {
            host.eq_ignore_ascii_case("github.com") || host.eq_ignore_ascii_case("github.com:443")
        })
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
    let (source, revision, into) = match parameters(site.call) {
        Ok(p) => p,
        Err(error) => return site.fail(state, &error),
    };
    if let Err(error) = free_destination(&state.conversation()?, into) {
        return site.fail(state, &error);
    }
    let invocation = ids::protocol_id(
        "git-import",
        &json!({
            "conversation":state.conversation()?.identity()?.id,
            "request":site.request, "round":site.round, "call":site.call.id,
            "parameters":site.call.input
        }),
    )?;
    let mut command = Command::new("caos");
    command.args(["import-git", source]);
    if let Some(revision) = revision {
        command.arg(revision);
    }
    command.args([format!("--invocation={invocation}"), "--json".into()]);
    if is_github_remote(source) && Path::new("/secret/github-token").exists() {
        command.arg("--github-token-file=/secret/github-token");
    }
    let output = command
        .output()
        .map_err(|e| format!("starting import-git: {e}"))?;
    if !output.status.success() {
        // Only this command's own fixed diagnostics are safe to show. Never
        // copy subprocess stderr, which a proxy or future Git caller may echo.
        return site.fail(
            state,
            "Remote Git import failed. Check the HTTPS repository, revision and token, then retry.",
        );
    }
    let result: Value =
        serde_json::from_slice(&output.stdout).map_err(|_| "invalid import-git result")?;
    let commit = Oid::parse(
        result["commit"]
            .as_str()
            .ok_or("import result lacks commit")?,
        "imported commit",
    )?;
    let provenance = json!({"repository":source, "default_branch":result["default_branch"],
        "requested_revision":revision, "commit":commit, "observed_at":result["observed_at"]});
    attach(state, site, into, &commit, &provenance)
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
