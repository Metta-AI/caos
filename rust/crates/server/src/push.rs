//! Push one pinned code commit directly from the server object store.
use crate::{Config, HttpError};
use conversation_protocol::v3::PublicationStatus;
use serde::{Deserialize, Deserializer};
use serde_json::json;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    destination: String,
    commit: String,
    branch: String,
    // No default: omission is different from an explicit create-only lease.
    #[serde(deserialize_with = "expected")]
    expected: Option<String>,
    #[serde(default)]
    rewrite: bool,
}

fn expected<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::deserialize(d)
}

pub(crate) fn endpoint(
    config: &Config,
    request: &mut tiny_http::Request,
) -> Result<Vec<u8>, HttpError> {
    let (mut input, token): (Input, _) = crate::remote_git::input(request)?;
    if !git_locator::import::commit(&input.commit)
        || input
            .expected
            .as_ref()
            .is_some_and(|h| !git_locator::import::commit(h))
    {
        return Err(HttpError::new(400, "push requires full commit hashes"));
    }
    input.commit.make_ascii_lowercase();
    if let Some(old) = &mut input.expected {
        old.make_ascii_lowercase();
    }
    let token = token.as_deref();
    git_locator::import::git(&input.destination, token).map_err(|e| HttpError::new(400, e))?;
    let run = |args: &[&str]| -> Result<std::process::Output, String> {
        git_locator::import::git(&input.destination, token)?
            .args(["--git-dir", &config.git_dir])
            .args(args)
            .output()
            .map_err(|_| "could not start Git publication".into())
    };
    let capture = |args: &[&str]| -> Result<String, String> {
        let output = run(args)?;
        if !output.status.success() {
            return Err("Git publication validation failed".into());
        }
        String::from_utf8(output.stdout).map_err(|_| "invalid Git response".into())
    };
    let refname = format!("refs/heads/{}", input.branch);
    if input.branch.starts_with('-')
        || !run(&["check-ref-format", &refname])
            .map_err(|e| HttpError::new(502, e))?
            .status
            .success()
    {
        return Err(HttpError::new(400, "invalid publication branch"));
    }
    let reject = |code: &str| HttpError::new(422, json!({"code":code}).to_string());
    let invalid = |_| reject("validation-failed");
    if capture(&["cat-file", "-t", &input.commit])
        .unwrap_or_default()
        .trim()
        != "commit"
    {
        return Err(reject("missing-commit"));
    }
    // Serialize this destination within the shared server store. A resumed
    // request waits for its peer's push instead of racing Git's remote ref lock.
    let locks = std::path::Path::new(&config.git_dir).join("caos-pushes");
    std::fs::create_dir_all(&locks)?;
    let key = format!(
        "{:x}",
        Sha256::digest(format!("{}\0{refname}", input.destination))
    );
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(locks.join(&key))?;
    lock.lock()?;

    let receipt =
        |status: PublicationStatus, observed: Option<String>, kind: &str, code: Option<&str>| {
            Ok(serde_json::to_vec(&json!({
                "commit": input.commit, "branch": input.branch, "status": status,
                "observed": observed, "kind": kind, "code": code,
                "diagnostic": code.and_then(git_locator::publish::diagnostic)
            }))
            .unwrap())
        };
    if let Some(old) = &input.expected {
        if capture(&["cat-file", "-t", old]).unwrap_or_default().trim() != "commit" {
            return Err(reject("missing-expected"));
        }
        if !input.rewrite {
            match run(&["merge-base", "--is-ancestor", old, &input.commit])
                .map_err(invalid)?
                .status
                .code()
            {
                Some(0) => {}
                Some(1) => return Err(reject("not-fast-forward")),
                _ => return Err(reject("validation-failed")),
            }
        }
    }
    if ignored_files(
        &input.destination,
        &config.git_dir,
        &input.commit,
        &locks.join(format!("{key}.check")),
    )
    .map_err(|_| reject("validation-failed"))?
    {
        return Err(reject("ignored-files"));
    }
    let lease = format!(
        "--force-with-lease={refname}:{}",
        input.expected.as_deref().unwrap_or("")
    );
    let refspec = format!("{}:{refname}", input.commit);
    let output = run(&[
        "-c",
        "push.followTags=false",
        "push",
        "--porcelain",
        "--no-verify",
        "--recurse-submodules=no",
        &lease,
        "--",
        &input.destination,
        &refspec,
    ]);
    match output {
        Ok(output) if output.status.success() => receipt(
            PublicationStatus::Complete,
            Some(input.commit.clone()),
            if String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.starts_with("=\t"))
            {
                "ref-converged"
            } else {
                "push-success"
            },
            None,
        ),
        Ok(output) => {
            if let Some(code) = rejection(&output.stdout, &refname) {
                receipt(
                    PublicationStatus::Conflict,
                    None,
                    if code == "lease-rejected" {
                        "lease-rejected"
                    } else {
                        "push-rejected"
                    },
                    Some(code),
                )
            } else {
                receipt(PublicationStatus::Uncertain, None, "ambiguous", None)
            }
        }
        Err(_) => receipt(PublicationStatus::Uncertain, None, "ambiguous", None),
    }
}

// The destination lock owns this scratch directory, including leftovers after
// a crash. Only an index and path list are written: skip-worktree lets Git read
// nested .gitignore blobs from the index without checking out source files.
fn ignored_files(
    destination: &str,
    git_dir: &str,
    commit: &str,
    directory: &std::path::Path,
) -> Result<bool, HttpError> {
    use std::fs::{self, File};
    if directory.exists() {
        fs::remove_dir_all(directory)?;
    }
    fs::create_dir_all(directory.join("work"))?;
    let directory = fs::canonicalize(directory)?;
    let result = (|| {
        let invalid = || HttpError::new(422, "Git ignore validation failed");
        let run = |args: &[&str], stdin: Option<File>| -> Result<Vec<u8>, HttpError> {
            let mut command = git_locator::import::git(destination, None).map_err(|_| invalid())?;
            command
                .args(["--git-dir", git_dir, "-c", "core.bare=false"])
                .args(["-c", "core.sparseCheckout=false", "--work-tree"])
                .arg(directory.join("work"))
                .env("GIT_INDEX_FILE", directory.join("index"))
                .args(args);
            if let Some(input) = stdin {
                command.stdin(input);
            }
            let output = command.output().map_err(|_| invalid())?;
            if !output.status.success() {
                return Err(invalid());
            }
            Ok(output.stdout)
        };
        run(&["read-tree", commit], None)?;
        // Only regular ignore files may supply patterns. Marking a symlink
        // skip-worktree would make Git's index fallback parse its link target.
        let mut patterns = Vec::new();
        for entry in run(&["ls-files", "--stage", "-z"], None)?
            .split(|b| *b == 0)
            .filter(|entry| entry.starts_with(b"100644 ") || entry.starts_with(b"100755 "))
        {
            let path = entry
                .splitn(2, |b| *b == b'\t')
                .nth(1)
                .ok_or_else(invalid)?;
            if path == b".gitignore" || path.ends_with(b"/.gitignore") {
                patterns.extend_from_slice(path);
                patterns.push(0);
            }
        }
        if patterns.is_empty() {
            return Ok(false);
        }
        let paths = directory.join("paths");
        fs::write(&paths, patterns)?;
        run(
            &["update-index", "--skip-worktree", "-z", "--stdin"],
            Some(File::open(paths)?),
        )?;
        // Explicit per-directory rules exclude host/global/info/exclude policy.
        // --cached deliberately checks tracked entries too: this is a publication
        // rule, stricter than Git's ordinary admission of untracked files.
        Ok(!run(
            &[
                "ls-files",
                "--cached",
                "--ignored",
                "--exclude-per-directory=.gitignore",
                "-z",
            ],
            None,
        )?
        .is_empty())
    })();
    fs::remove_dir_all(directory)?;
    result
}

// Only a per-ref porcelain rejection proves the receiver refused this update.
// Missing status (including authentication/transport failures) remains uncertain.
fn rejection(stdout: &[u8], refname: &str) -> Option<&'static str> {
    String::from_utf8_lossy(stdout).lines().find_map(|line| {
        let mut fields = line.split('\t');
        if fields.next()? != "!" || fields.next()?.rsplit_once(':')?.1 != refname {
            return None;
        }
        let reason = fields.next()?;
        // Git labels a missing report as remote rejected even though the
        // receiver may already have accepted the update.
        if reason.ends_with("(remote failed to report status)") {
            return None;
        }
        if !reason.starts_with("[rejected]") && !reason.starts_with("[remote rejected]") {
            return None;
        }
        Some(
            if reason.ends_with("(stale info)")
                || reason.ends_with("(incorrect old value provided)")
            {
                "lease-rejected"
            } else if reason.ends_with("(pre-receive hook declined)") {
                "hook-declined"
            } else {
                "remote-rejected"
            },
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expected_is_required_but_may_be_null() {
        let mut value =
            json!({"destination":"https://host/repo","commit":"a".repeat(40),"branch":"topic"});
        assert!(serde_json::from_value::<Input>(value.clone()).is_err());
        value["expected"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Input>(value.clone())
            .unwrap()
            .expected
            .is_none());
        value["expected"] = json!("b".repeat(40));
        assert!(serde_json::from_value::<Input>(value)
            .unwrap()
            .expected
            .is_some());
    }
}
