//! Secrets: identity-is-capability injection (design/secrets.md).
//!
//! A secret is a value plus a list of *readers* — partial arg trees allowed to
//! see it. A job may read a secret iff its ArgTree is a **superset** of one of
//! the readers: the reader states the minimum that must hold, unspecified args
//! are wildcards. The value never enters the ArgTree or the cache key (secrets
//! are computed here, at dispatch, from the job's already-built arg entries),
//! so rotating a value busts no cache and the value is never content-addressed.
//! This is identity-is-capability: what may read `github` is decided by *what
//! the code is* (its arg entries, matched by oid), not by a token it holds.
//!
//! Secrets live in a git-ignored directory named by `CAOS_SECRETS_DIR` (unset
//! or missing ⇒ no secrets). One file per secret, its name the secret name, in
//! the repeated-key form:
//!
//! ```text
//! # a comment
//! value=ghp_realtokenbytes…      # or value:@=token.pem to read a sibling file
//! reader=std/github-push
//! reader=std/github-push -- --repo=github.com/me/proj
//! ```
//!
//! Each `reader` is a `std/<name>` image path, optionally pinning literal
//! `--k=v` args; it is resolved (against the job's own `std`) to a partial set
//! of arg entries and subset-matched against the job. (Tree-relative reader
//! paths and `:@=` reader args are not resolved yet — such a reader is skipped
//! with a warning, so it never grants; fail closed.)
//!
//! The matched `(name, value)` pairs ride out in the job payload (out of band,
//! never an arg) and the container runner drops them at `/secret/<name>` for
//! the worker (see `crates/caos/src/bin/caos.rs`).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::storage::store_git_blob;
use crate::Config;

/// Env var naming the git-ignored secrets directory. Unset/empty/missing ⇒ no
/// secrets are ever injected.
const SECRETS_DIR_ENV: &str = "CAOS_SECRETS_DIR";

/// A parsed secret file: the secret's value and its readers (raw reader
/// expressions, resolved per job against that job's `std`).
struct Secret {
    name: String,
    value: String,
    readers: Vec<String>,
}

/// The secrets visible to a job whose top-level arg entries are `arg_entries`
/// and whose std tree is `std`: every secret with at least one reader whose
/// resolved partial arg tree is a subset of `arg_entries`. Deduped by name.
///
/// Best-effort and fail-closed: a directory that can't be read, a secret that
/// can't be parsed, or a reader that can't be resolved is logged and grants
/// nothing — a secret is injected only when a reader positively matches.
pub(crate) fn for_job(
    config: &Config,
    std: &str,
    arg_entries: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let dir = match std::env::var(SECRETS_DIR_ENV) {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => return Vec::new(),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for secret in read_secrets(&dir) {
        let visible = secret.readers.iter().any(|reader| {
            match resolve_reader(config, std, reader) {
                Ok(entries) => is_subset(&entries, arg_entries),
                Err(e) => {
                    eprintln!("secret {}: ignoring reader {reader:?}: {e}", secret.name);
                    false
                }
            }
        });
        if visible && seen.insert(secret.name.clone()) {
            eprintln!("secret {}: granted to this job", secret.name);
            out.push((secret.name, secret.value));
        }
    }
    out
}

/// Read and parse every secret file in `dir` (one file per secret). A dir that
/// doesn't exist yields no secrets; an unparseable file is skipped with a
/// warning.
fn read_secrets(dir: &Path) -> Vec<Secret> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut secrets = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skip dotfiles (editor backups, `.gitignore`) and anything that isn't
        // a plain file.
        if name.starts_with('.') || !path.is_file() {
            continue;
        }
        match parse_secret(&name, &path) {
            Ok(secret) => secrets.push(secret),
            Err(e) => eprintln!("secrets: skipping {name}: {e}"),
        }
    }
    secrets
}

/// Parse one secret file in the repeated-key form. `value=`/`value:@=` set the
/// value (the latter reads a sibling file's bytes); `reader=` accumulates.
fn parse_secret(name: &str, path: &Path) -> Result<Secret, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {name}: {e}"))?;
    let mut value: Option<String> = None;
    let mut readers = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, val) = line
            .split_once('=')
            .ok_or_else(|| format!("line {line:?} is not key=value"))?;
        match key {
            "value" => value = Some(val.to_string()),
            "value:@" => {
                let dir = path.parent().unwrap_or_else(|| Path::new("."));
                let file = dir.join(val);
                let bytes = std::fs::read(&file).map_err(|e| format!("value:@={val}: {e}"))?;
                value = Some(
                    String::from_utf8(bytes)
                        .map_err(|e| format!("value:@={val}: not UTF-8: {e}"))?,
                );
            }
            "reader" => readers.push(val.trim().to_string()),
            other => return Err(format!("unknown key {other:?}")),
        }
    }
    let value = value.ok_or("no value= line")?;
    Ok(Secret {
        name: name.to_string(),
        value,
        readers,
    })
}

/// Resolve a reader expression (`std/<name> [-- --k=v …]`) to the partial arg
/// entries it pins: `image` → the std image's oid, plus a blob-oid entry per
/// literal `--k=v`. These are subset-matched against a job's arg entries.
fn resolve_reader(
    config: &Config,
    std: &str,
    reader: &str,
) -> Result<BTreeMap<String, String>, String> {
    let tokens: Vec<&str> = reader.split_whitespace().collect();
    let path = *tokens.first().ok_or("empty reader")?;
    let std_name = path
        .strip_prefix("/std/")
        .or_else(|| path.strip_prefix("std/"))
        .ok_or_else(|| {
            format!("reader path {path:?} is not std/<name> (only std readers resolve for now)")
        })?;
    if std_name.is_empty() || std_name.contains('/') {
        return Err(format!("reader path {path:?} must be std/<name>"));
    }
    let image_oid = crate::compute::std_image(config, std, std_name)
        .map_err(|e| format!("resolving {path}: {}", e.message()))?;
    let mut entries = BTreeMap::new();
    entries.insert("image".to_string(), image_oid);

    let mut rest = tokens.iter().skip(1);
    if let Some(&sep) = rest.next() {
        if sep != "--" {
            return Err(format!("expected `--` before reader args, got {sep:?}"));
        }
        for &tok in rest {
            let body = tok
                .strip_prefix("--")
                .ok_or_else(|| format!("reader arg {tok:?} must be --name=value"))?;
            let (key, val) = body
                .split_once('=')
                .ok_or_else(|| format!("reader arg {tok:?} must be --name=value"))?;
            if key.contains(':') {
                return Err(format!(
                    "reader arg {tok:?}: only literal --name=value is supported (not :@=)"
                ));
            }
            if key.is_empty() || key.contains('/') {
                return Err(format!(
                    "reader arg name {key:?} must be one path component"
                ));
            }
            let oid = store_git_blob(config, val.as_bytes())
                .map_err(|e| format!("storing reader literal {val:?}: {e}"))?
                .to_string();
            entries.insert(key.to_string(), oid);
        }
    }
    Ok(entries)
}

/// Is `reader` (a partial arg tree) a subset of `job`? Every (name, oid) the
/// reader pins must appear identically in the job — pure oid equality, the same
/// match the runner rendezvous uses.
fn is_subset(reader: &BTreeMap<String, String>, job: &BTreeMap<String, String>) -> bool {
    reader.iter().all(|(name, oid)| job.get(name) == Some(oid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subset_matches_only_when_every_pinned_entry_is_present() {
        let job: BTreeMap<String, String> = [
            ("image", "aa"),
            ("std", "bb"),
            ("worker1", "cc"),
            ("repo", "dd"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        // Just the image: a superset job matches (extra args are wildcards).
        let only_image: BTreeMap<String, String> = [("image", "aa")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert!(is_subset(&only_image, &job));

        // A pinned arg that agrees still matches.
        let image_and_repo: BTreeMap<String, String> = [("image", "aa"), ("repo", "dd")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert!(is_subset(&image_and_repo, &job));

        // A pinned arg that disagrees does not.
        let wrong_repo: BTreeMap<String, String> = [("image", "aa"), ("repo", "zz")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert!(!is_subset(&wrong_repo, &job));

        // A pinned arg the job lacks does not.
        let extra: BTreeMap<String, String> = [("image", "aa"), ("marker", "xyz")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert!(!is_subset(&extra, &job));

        // A different image never matches.
        let other_image: BTreeMap<String, String> = [("image", "ff")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert!(!is_subset(&other_image, &job));
    }

    #[test]
    fn parse_secret_reads_value_and_readers() {
        let dir = std::env::temp_dir().join(format!("caos-secret-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("github");
        std::fs::write(
            &path,
            "# prod token\nvalue=ghp_abc123\nreader=std/github-push\n\
             reader=std/github-push -- --repo=x\n",
        )
        .unwrap();
        let secret = parse_secret("github", &path).unwrap();
        assert_eq!(secret.name, "github");
        assert_eq!(secret.value, "ghp_abc123");
        assert_eq!(
            secret.readers,
            vec![
                "std/github-push".to_string(),
                "std/github-push -- --repo=x".to_string()
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_secret_requires_a_value() {
        let dir = std::env::temp_dir().join(format!("caos-secret-noval-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty");
        std::fs::write(&path, "reader=std/bash\n").unwrap();
        assert!(parse_secret("empty", &path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
