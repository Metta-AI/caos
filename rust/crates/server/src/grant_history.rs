//! The roots a `reader:@@=` grant allows (SPEC.md, "Secrets"): the tree of
//! the locator's `rev` and of each first-parent ancestor back to `since`.
//!
//! Only commits are needed — a commit names its tree — so a repository this
//! server does not already hold is fetched with `--filter=tree:0` into a
//! staging repo of its own, never into the object database, whose closure
//! check refuses a partial history.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use git_locator::GitRef;

use crate::Config;

type Key = (String, String, Option<String>);
static ALLOWED: Mutex<Option<HashMap<Key, Arc<HashSet<String>>>>> = Mutex::new(None);

pub(crate) fn allowed_roots(
    config: &Config,
    git_ref: &GitRef,
    since: Option<&str>,
) -> Result<Arc<HashSet<String>>, String> {
    let rev = git_ref
        .rev
        .clone()
        .ok_or("a reader:@@= locator must pin rev=<sha>")?;
    let key = (git_ref.fetch_url(), rev.clone(), since.map(str::to_string));
    if let Some(hit) = ALLOWED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(&key).cloned())
    {
        return Ok(hit);
    }
    let roots = match first_parent_trees(&config.git_dir, &rev, since) {
        Ok(roots) => roots,
        Err(_) => {
            let staging = staging_repo(config, &git_ref.fetch_url())?;
            git(
                &staging,
                &["fetch", "-q", "--filter=tree:0", "origin", &rev],
            )?;
            first_parent_trees(&staging.to_string_lossy(), &rev, since)?
        }
    };
    let roots = Arc::new(roots);
    ALLOWED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(key, roots.clone());
    Ok(roots)
}

fn first_parent_trees(
    dir: &str,
    rev: &str,
    since: Option<&str>,
) -> Result<HashSet<String>, String> {
    let dir = Path::new(dir);
    let mut args = vec!["log", "--first-parent", "--format=%T", rev];
    let exclude;
    if let Some(since) = since {
        git(dir, &["merge-base", "--is-ancestor", since, rev])
            .map_err(|_| format!("since={since} is not an ancestor of {rev}"))?;
        exclude = format!("^{since}");
        args.push(&exclude);
    }
    let mut roots: HashSet<String> = git(dir, &args)?.lines().map(str::to_string).collect();
    if let Some(since) = since {
        roots.insert(
            git(dir, &["log", "-1", "--format=%T", since])?
                .trim()
                .to_string(),
        );
    }
    Ok(roots)
}

/// A bare repo per URL, beside `secrets.git`, set up as a treeless partial
/// clone of it.
fn staging_repo(config: &Config, url: &str) -> Result<PathBuf, String> {
    let digest =
        gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, url.as_bytes())
            .map_err(|e| format!("hashing {url}: {e}"))?;
    let dir = Path::new(&config.secrets_git)
        .with_file_name("grant-history")
        .join(format!("{digest}.git"));
    if !dir.join("HEAD").is_file() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        git(&dir, &["init", "-q", "--bare"])?;
        git(&dir, &["remote", "add", "origin", url])?;
        git(&dir, &["config", "remote.origin.promisor", "true"])?;
        git(
            &dir,
            &["config", "remote.origin.partialclonefilter", "tree:0"],
        )?;
        git(&dir, &["config", "extensions.partialClone", "origin"])?;
    }
    Ok(dir)
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("running git {args:?}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_parent_back_to_since() {
        let dir = std::env::temp_dir().join(format!(
            "caos-grant-history-{}-{:?}",
            std::process::id(),
            std::time::Instant::now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| git(&dir, args).unwrap();
        run(&["init", "-q", "-b", "main"]);
        let commit = |msg: &str| {
            std::fs::write(dir.join("f"), msg).unwrap();
            run(&["add", "f"]);
            run(&[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                msg,
            ]);
            run(&["rev-parse", "HEAD"]).trim().to_string()
        };
        let tree = |rev: &str| {
            run(&["rev-parse", &format!("{rev}^{{tree}}")])
                .trim()
                .to_string()
        };
        let a = commit("a");
        run(&["checkout", "-q", "-b", "side"]);
        let side = commit("side");
        run(&["checkout", "-q", "main"]);
        let b = commit("b");
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "merge",
            "-q",
            "--no-edit",
            "-X",
            "ours",
            "side",
        ]);
        let c = run(&["rev-parse", "HEAD"]).trim().to_string();

        let d = dir.to_str().unwrap();
        let roots = first_parent_trees(d, &c, Some(&b)).unwrap();
        assert!(roots.contains(&tree(&c)) && roots.contains(&tree(&b)));
        assert!(!roots.contains(&tree(&a)), "before since");
        assert!(
            !roots.contains(&tree(&side)),
            "a merged branch is not first-parent"
        );
        assert!(first_parent_trees(d, &c, None).unwrap().contains(&tree(&a)));
        assert!(first_parent_trees(d, &b, Some(&side)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
