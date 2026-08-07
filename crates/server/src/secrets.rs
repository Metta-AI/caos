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
use std::process::Command;

use gix::objs::tree::EntryMode;
use gix::ObjectId;

use crate::storage::{fetch_blob, fetch_tree, store_git_blob};
use crate::Config;

/// Env var naming the git-ignored secrets directory. Unset/empty/missing ⇒ no
/// secrets are ever injected.
const SECRETS_DIR_ENV: &str = "CAOS_SECRETS_DIR";

/// Env var (or the `.tree` file in the secrets dir, which wins) naming **the
/// current tree** tree-relative readers resolve against: a tree/commit hash or
/// a git ref. Unset ⇒ only `std/<name>` readers resolve (design/secrets.md:
/// "eval the grant against the same tree revision the work runs from").
const SECRETS_TREE_ENV: &str = "CAOS_SECRETS_TREE";

/// A parsed secret file: the secret's value and its readers (raw reader
/// expressions, resolved per job against that job's `std`).
///
/// Readers are kept UNRESOLVED — as their source strings — on purpose:
/// resolution is std-relative (a `std/<name>` reader resolves to a different
/// oid under a different `std`), so a reader's arg entries are a function of
/// *(reader expr, the job's std)*, not a property of the secret. There is no
/// single resolved tree to store here; resolution happens at match time in
/// [`for_job`]. (Re-reading the store each dispatch is also what lets an edit —
/// a rotated value, an added or revoked grant — take effect on the next job.)
struct Secret {
    name: String,
    value: String,
    readers: Vec<String>,
}

/// The secrets visible to a job whose ArgTree's top-level entries are
/// `arg_entries` (name → oid, as `compute::args_entries` reads them) and whose
/// std tree is `std`: every secret with at least one reader whose resolved
/// partial arg tree is a subset of those entries. Deduped by name.
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
    // The current tree tree-relative readers resolve against (design/secrets.md).
    // Resolved once per dispatch — so, like the store itself, an edit takes
    // effect on the next job.
    let tree = current_tree(config, &dir);
    for secret in read_secrets(&dir) {
        let visible = secret.readers.iter().any(|reader| {
            match resolve_reader(config, std, tree.as_deref(), reader) {
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

/// Resolve a reader expression to the partial arg entries it pins, then
/// subset-matched against a job. A reader is `<image> [-- --k=v | --k:@=path …]`
/// where `<image>` is either:
///
/// - `std/<name>` (or `/std/<name>`) — the std image, resolved against the job's
///   own `std`; or
/// - a path in the **current tree** (`current_tree`) — evaluated like
///   `eval-path`: a tool dir with a `curry`-form `.caos-expr` contributes that
///   curry's image + bound args (so a repo tool's identity — usually a base
///   image plus a curried `worker1` script — is captured, not just its dir
///   oid), and a plain image dir contributes its subtree oid as `image`.
///
/// (`run`-form and variable `.caos-expr` files aren't resolved here — a grant
/// must not trigger compute — so such a reader is an error and grants nothing.)
fn resolve_reader(
    config: &Config,
    std: &str,
    current_tree: Option<&str>,
    reader: &str,
) -> Result<BTreeMap<String, String>, String> {
    let tokens: Vec<&str> = reader.split_whitespace().collect();
    let path = *tokens.first().ok_or("empty reader")?;

    let mut entries = if let Some(std_name) = path
        .strip_prefix("/std/")
        .or_else(|| path.strip_prefix("std/"))
    {
        if std_name.is_empty() || std_name.contains('/') {
            return Err(format!("reader path {path:?} must be std/<name>"));
        }
        let image_oid = std_image(config, std, std_name)?;
        BTreeMap::from([("image".to_string(), image_oid)])
    } else {
        let tree = current_tree.ok_or_else(|| {
            format!(
                "reader path {path:?} is tree-relative but no current tree is configured \
                 (set {SECRETS_TREE_ENV} or a `.tree` file in the secrets dir)"
            )
        })?;
        eval_tree_path(config, std, tree, path)?
    };

    // The reader's own trailing `-- --k=v` literal pins (narrow the match).
    let mut rest = tokens.iter().skip(1);
    if let Some(&sep) = rest.next() {
        if sep != "--" {
            return Err(format!("expected `--` before reader args, got {sep:?}"));
        }
        for &tok in rest {
            let (key, oid) = literal_arg(config, tok)?;
            entries.insert(key, oid);
        }
    }
    Ok(entries)
}

/// Resolve a tree-relative reader path within `tree` to its partial arg entries,
/// the eval-path way (see [`resolve_reader`]).
fn eval_tree_path(
    config: &Config,
    std: &str,
    tree: &str,
    path: &str,
) -> Result<BTreeMap<String, String>, String> {
    let (mode, oid) = lookup_in_tree(config, tree, path)?
        .ok_or_else(|| format!("reader path {path:?} not found in the current tree"))?;
    // A file (or any non-tree) can only be an `image` by its own oid.
    if !mode.is_tree() {
        return Ok(BTreeMap::from([("image".to_string(), oid.to_string())]));
    }
    let subtree = oid.to_string();
    // A `curry`-form `.caos-expr` at the dir root defines the tool's ArgTree.
    if let Some((expr_mode, expr_oid)) = lookup_in_tree(config, &subtree, ".caos-expr")? {
        if !expr_mode.is_tree() {
            return eval_curry_expr(config, std, &subtree, &expr_oid.to_string());
        }
    }
    // A plain image dir: its subtree oid is the `image`.
    Ok(BTreeMap::from([("image".to_string(), subtree)]))
}

/// Evaluate a single `curry <image> -- [--k=v | --k:@=path]` `.caos-expr` blob
/// against `subtree` (the dir it sits in), returning the curry's partial arg
/// entries. Only the single-line curry form is handled; anything else (a `run`,
/// a `NAME=` variable, multiple lines) is an error — a grant never computes.
fn eval_curry_expr(
    config: &Config,
    std: &str,
    subtree: &str,
    expr_oid: &str,
) -> Result<BTreeMap<String, String>, String> {
    let bytes = fetch_blob(config, expr_oid).map_err(|e| format!("reading .caos-expr: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|e| format!(".caos-expr not UTF-8: {e}"))?;
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let line = lines.next().ok_or(".caos-expr reader has no expression")?;
    if lines.next().is_some() {
        return Err("only a single-line `curry` .caos-expr reader is resolved".to_string());
    }
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.first() != Some(&"curry") {
        return Err(
            "only `curry`-form .caos-expr readers are resolved (a `run`/variable form \
             would compute)"
                .to_string(),
        );
    }
    let sep = tokens
        .iter()
        .position(|&t| t == "--")
        .ok_or("`curry` .caos-expr needs `--` before its args")?;
    if sep != 2 {
        return Err("`curry` .caos-expr takes exactly one image before `--`".to_string());
    }
    let mut entries = BTreeMap::new();
    entries.insert(
        "image".to_string(),
        resolve_expr_image(config, std, subtree, tokens[1])?,
    );
    for &tok in &tokens[sep + 1..] {
        let (key, oid) = expr_arg(config, std, subtree, tok)?;
        entries.insert(key, oid);
    }
    Ok(entries)
}

/// Resolve a `curry` expression's image token: `/std/<name>`, a bare hash, or a
/// path within `subtree`.
fn resolve_expr_image(
    config: &Config,
    std: &str,
    subtree: &str,
    tok: &str,
) -> Result<String, String> {
    if let Some(name) = tok
        .strip_prefix("/std/")
        .or_else(|| tok.strip_prefix("std/"))
    {
        return std_image(config, std, name);
    }
    if is_hex40(tok) {
        return Ok(tok.to_string());
    }
    let (mode, oid) = lookup_in_tree(config, subtree, tok)?
        .ok_or_else(|| format!("image path {tok:?} not found in reader dir"))?;
    if !mode.is_tree() {
        return Err(format!("image path {tok:?} is not a directory"));
    }
    Ok(oid.to_string())
}

/// Resolve one `--name=value` / `--name:@=path` arg of a `curry` `.caos-expr`
/// (paths relative to `subtree`, `/std/<name>` allowed for `:@=`), to a
/// `(name, oid)` entry.
fn expr_arg(
    config: &Config,
    std: &str,
    subtree: &str,
    tok: &str,
) -> Result<(String, String), String> {
    let body = tok
        .strip_prefix("--")
        .ok_or_else(|| format!("expected --name=value, got {tok:?}"))?;
    let (key, value) = body
        .split_once('=')
        .ok_or_else(|| format!("expected --name[:@]=value, got {tok:?}"))?;
    let (name, is_path) = match key.split_once(':') {
        None => (key, false),
        Some((n, "@")) => (n, true),
        Some((_, ty)) => return Err(format!("unknown arg type {ty:?} in {tok:?}")),
    };
    if name.is_empty() || name.contains('/') {
        return Err(format!("arg name {name:?} must be one path component"));
    }
    let oid = if is_path {
        if let Some(std_name) = value
            .strip_prefix("/std/")
            .or_else(|| value.strip_prefix("std/"))
        {
            std_image(config, std, std_name)?
        } else {
            lookup_in_tree(config, subtree, value)?
                .ok_or_else(|| format!("path {value:?} not found in reader dir"))?
                .1
                .to_string()
        }
    } else {
        store_git_blob(config, value.as_bytes())
            .map_err(|e| format!("storing literal {value:?}: {e}"))?
            .to_string()
    };
    Ok((name.to_string(), oid))
}

/// Resolve a reader's own trailing literal `--name=value` arg to a `(name, oid)`
/// entry (only literals; `:@=` reader args aren't supported).
fn literal_arg(config: &Config, tok: &str) -> Result<(String, String), String> {
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
    Ok((key.to_string(), oid))
}

/// Look up `rel` (a `/`-separated path) within `tree`, returning the entry's
/// `(mode, oid)` — `None` if any component is missing or a non-final component
/// isn't a directory. An empty `rel` is the tree itself.
fn lookup_in_tree(
    config: &Config,
    tree: &str,
    rel: &str,
) -> Result<Option<(EntryMode, ObjectId)>, String> {
    let comps: Vec<&str> = rel
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if comps.is_empty() {
        let oid =
            ObjectId::from_hex(tree.as_bytes()).map_err(|e| format!("bad tree {tree}: {e}"))?;
        return Ok(Some((gix::objs::tree::EntryKind::Tree.into(), oid)));
    }
    let mut current = tree.to_string();
    for (idx, comp) in comps.iter().enumerate() {
        let entries = fetch_tree(config, &current)?;
        let Some(entry) = entries.into_iter().find(|e| e.name == *comp) else {
            return Ok(None);
        };
        if idx == comps.len() - 1 {
            return Ok(Some((entry.mode, entry.oid)));
        }
        if !entry.mode.is_tree() {
            return Ok(None);
        }
        current = entry.oid.to_string();
    }
    unreachable!("returns on the last component")
}

/// The current tree tree-relative readers resolve against: a `.tree` file in the
/// secrets dir (wins) or the [`SECRETS_TREE_ENV`] env, each a tree/commit hash
/// or git ref, resolved to a tree hash. `None` if neither is set or it can't be
/// resolved (tree-relative readers then grant nothing).
fn current_tree(config: &Config, dir: &Path) -> Option<String> {
    let spec = std::fs::read_to_string(dir.join(".tree"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var(SECRETS_TREE_ENV)
                .ok()
                .filter(|s| !s.is_empty())
        })?;
    // `<spec>^{tree}` resolves a ref, a commit, or a tree uniformly to its tree.
    let out = Command::new("git")
        .args([
            "-C",
            &config.git_dir,
            "rev-parse",
            &format!("{spec}^{{tree}}"),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!("secrets: cannot resolve current tree {spec:?}");
        return None;
    }
    let hash = String::from_utf8(out.stdout).ok()?.trim().to_string();
    is_hex40(&hash).then_some(hash)
}

/// A 40-char lowercase-or-mixed hex sha1.
fn is_hex40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Look up a named std image, adapting the error to a `String`.
fn std_image(config: &Config, std: &str, name: &str) -> Result<String, String> {
    crate::compute::std_image(config, std, name).map_err(|e| e.message().to_string())
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
