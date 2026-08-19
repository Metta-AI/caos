//! Direct Git transport for the few workers whose runtime explicitly includes
//! Git. This is client-side repository plumbing, not a wrapper around
//! CAOS-specific ref endpoints: reads are exact fetches and writes are ordinary
//! pushes with explicit leases.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::scratch;

const REMOTE: &str = "origin";

/// One compare-and-swap update in an atomic push. `expected = None` means the
/// ref must be absent; `new = None` is a deletion (also useful for making an
/// absence lease participate in a multi-ref transaction).
#[derive(Clone, Copy)]
pub struct RefUpdate<'a> {
    pub refname: &'a str,
    pub expected: Option<&'a str>,
    pub new: Option<&'a str>,
}

/// A throwaway bare repository connected to the CAOS server's ordinary Git
/// smart-HTTP transport.
pub struct Repo {
    path: PathBuf,
}

impl Repo {
    /// Create a fresh repository under `/tmp/<scratch_name>` and add
    /// `CAOS_SERVER_URL` as its origin. The containing worker image, not this
    /// library, is responsible for providing `git`.
    pub fn new(scratch_name: &str) -> Result<Self, String> {
        let path = scratch(scratch_name)?;
        let server =
            std::env::var("CAOS_SERVER_URL").map_err(|_| "CAOS_SERVER_URL not set".to_string())?;
        let init = git_output_at(None, &["init", "--bare", "--quiet", path_str(&path)?])?;
        require_success("initializing Git repository", &init)?;
        let repo = Self { path };
        let remote = repo.output(&["remote", "add", REMOTE, server.trim_end_matches('/')])?;
        require_success("adding CAOS Git remote", &remote)?;
        let protocol = repo.output(&["config", "protocol.version", "2"])?;
        require_success("selecting Git protocol v2", &protocol)?;
        let gc = repo.output(&["config", "gc.auto", "0"])?;
        require_success("disabling scratch repository GC", &gc)?;
        Ok(repo)
    }

    /// Fetch one exact remote ref and return its object id. The ordinary path
    /// uses protocol-v2 fetch, whose ref-prefix is the exact requested name.
    /// Only a failed fetch is followed by `ls-remote --exit-code` so absence can
    /// be distinguished from a transport failure without parsing diagnostics.
    pub fn read_ref(&self, refname: &str) -> Result<Option<String>, String> {
        self.validate_ref(refname)?;
        match fs::remove_file(self.path.join("FETCH_HEAD")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("clearing FETCH_HEAD: {error}")),
        }
        let fetched = self.output(&[
            "fetch",
            "--quiet",
            "--no-tags",
            "--depth=1",
            "--filter=tree:0",
            REMOTE,
            refname,
        ])?;
        if fetched.status.success() {
            let head = self.output(&["rev-parse", "--verify", "FETCH_HEAD"])?;
            require_success("reading FETCH_HEAD", &head)?;
            return parse_oid(&head.stdout, "fetched ref").map(Some);
        }

        let probe = self.output(&["ls-remote", "--exit-code", "--refs", REMOTE, refname])?;
        if probe.status.code() == Some(2) {
            return Ok(None);
        }
        if probe.status.success() {
            // The ref exists, so the exact fetch failed for a real reason.
            return Err(output_error("fetching exact remote ref", &fetched));
        }
        Err(format!(
            "{}; absence probe also failed: {}",
            output_error("fetching exact remote ref", &fetched),
            output_error("probing remote ref", &probe)
        ))
    }

    /// Push one ref update with an explicit lease. A nonzero result is left to
    /// the caller to interpret by rereading the ref: it may be a clean race, a
    /// lost success response, or an infrastructure failure.
    pub fn push_ref(&self, refname: &str, expected: Option<&str>, new: &str) -> Result<(), String> {
        self.push(
            &[RefUpdate {
                refname,
                expected,
                new: Some(new),
            }],
            false,
        )
    }

    /// Push several ref updates as one receive-pack transaction, with one
    /// explicit lease per ref.
    pub fn push_atomic(&self, updates: &[RefUpdate<'_>]) -> Result<(), String> {
        if updates.is_empty() {
            return Err("atomic Git push needs at least one ref update".to_string());
        }
        self.push(updates, true)
    }

    fn push(&self, updates: &[RefUpdate<'_>], atomic: bool) -> Result<(), String> {
        for update in updates {
            self.validate_ref(update.refname)?;
            if let Some(expected) = update.expected {
                validate_oid(expected, "expected ref")?;
            }
            if let Some(new) = update.new {
                validate_oid(new, "new ref")?;
                self.ensure_object(new)?;
            }
        }

        let mut args = vec!["push".to_string(), "--quiet".to_string()];
        if atomic {
            args.push("--atomic".to_string());
        }
        for update in updates {
            args.push(format!(
                "--force-with-lease={}:{}",
                update.refname,
                update.expected.unwrap_or_default()
            ));
        }
        args.push(REMOTE.to_string());
        for update in updates {
            args.push(match update.new {
                Some(new) => format!("{new}:{}", update.refname),
                None => format!(":{}", update.refname),
            });
        }
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let pushed = self.output(&refs)?;
        require_success("pushing Git ref update", &pushed)
    }

    /// Ensure an object already stored through the core object API is present
    /// in this scratch ODB, so send-pack can use it as a refspec source.
    fn ensure_object(&self, oid: &str) -> Result<(), String> {
        let object = format!("{oid}^{{object}}");
        let present = self.output(&["cat-file", "-e", &object])?;
        if present.status.success() {
            return Ok(());
        }
        let fetched = self.output(&[
            "fetch",
            "--quiet",
            "--no-tags",
            "--depth=1",
            "--filter=tree:0",
            REMOTE,
            oid,
        ])?;
        require_success(&format!("fetching push object {oid}"), &fetched)
    }

    fn validate_ref(&self, refname: &str) -> Result<(), String> {
        let checked = self.output(&["check-ref-format", refname])?;
        if checked.status.success() {
            Ok(())
        } else {
            Err(format!("invalid Git refname {refname:?}"))
        }
    }

    fn output(&self, args: &[&str]) -> Result<Output, String> {
        git_output_at(Some(&self.path), args)
    }
}

fn git_output_at(repo: Option<&Path>, args: &[&str]) -> Result<Output, String> {
    let mut command = Command::new("git");
    if let Some(repo) = repo {
        command.arg("-C").arg(repo);
    }
    command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    command
        .output()
        .map_err(|error| format!("running git {}: {error}", args.join(" ")))
}

fn require_success(action: &str, output: &Output) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        Err(output_error(action, output))
    }
}

fn output_error(action: &str, output: &Output) -> String {
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if detail.is_empty() {
        format!("{action}: git exited with {}", output.status)
    } else {
        format!("{action}: git exited with {}: {detail}", output.status)
    }
}

fn parse_oid(stdout: &[u8], what: &str) -> Result<String, String> {
    let value = std::str::from_utf8(stdout)
        .map_err(|error| format!("{what} is not UTF-8: {error}"))?
        .trim();
    validate_oid(value, what)?;
    Ok(value.to_string())
}

fn validate_oid(oid: &str, what: &str) -> Result<(), String> {
    if oid.len() == 40
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(format!(
            "{what} is not a 40-character Git object id: {oid:?}"
        ))
    }
}

fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("Git repository path is not UTF-8: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_validation_is_shape_only_and_transport_agnostic() {
        assert!(validate_oid(&"a".repeat(40), "oid").is_ok());
        assert!(validate_oid(&"A".repeat(40), "oid").is_err());
        assert!(validate_oid(&"a".repeat(39), "oid").is_err());
        assert!(validate_oid(&"z".repeat(40), "oid").is_err());
    }
}
