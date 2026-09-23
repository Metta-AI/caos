//! Worktree-free Git operations over objects already stored on the server.
use crate::{Config, HttpError};
use serde::Deserialize;
use std::process::Command;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeInput {
    merge_base: String,
    ours: String,
    theirs: String,
}

pub(crate) fn merge_endpoint(
    config: &Config,
    request: &mut tiny_http::Request,
) -> Result<Vec<u8>, HttpError> {
    let (input, _): (MergeInput, _) = crate::remote_git::input(request)?;
    merge(config, &input)
}

fn merge(config: &Config, input: &MergeInput) -> Result<Vec<u8>, HttpError> {
    let repo = config.repo.to_thread_local();
    let mut trees = Vec::new();
    for hash in [&input.merge_base, &input.ours, &input.theirs] {
        if !git_locator::import::commit(hash) {
            return Err(HttpError::new(400, "merge-tree requires full tree hashes"));
        }
        let id = gix::ObjectId::from_hex(hash.as_bytes())
            .map_err(|_| HttpError::new(400, "invalid tree hash"))?;
        let header = repo
            .find_header(id)
            .map_err(|_| HttpError::new(400, format!("missing tree {id}")))?;
        if header.kind() != gix::object::Kind::Tree {
            return Err(HttpError::new(400, format!("{id} is not a tree")));
        }
        trees.push(id.to_string());
    }
    let mut command = Command::new("git");
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    let result = command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .args([
            "-c",
            "core.fsync=objects",
            "--git-dir",
            &config.git_dir,
            "merge-tree",
            "--write-tree",
        ])
        .arg(format!("--merge-base={}", trees[0]))
        .args([&trees[1], &trees[2]])
        .output()?;
    // Exit 1 is a usable merged tree with conflicts, not a failed operation.
    if !matches!(result.status.code(), Some(0 | 1)) {
        return Err(HttpError::new(
            500,
            format!(
                "git merge-tree failed: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            ),
        ));
    }
    let output = String::from_utf8(result.stdout)
        .map_err(|_| HttpError::new(500, "merge-tree returned non-UTF-8 output"))?;
    let (tree, conflicts) = output
        .split_once('\n')
        .ok_or_else(|| HttpError::new(500, "merge-tree returned no tree"))?;
    if !git_locator::import::commit(tree) {
        return Err(HttpError::new(500, "merge-tree returned an invalid tree"));
    }
    crate::storage::head_object(config, tree)?;
    Ok(serde_json::to_vec(&serde_json::json!({
        "tree": tree, "conflicts": conflicts
    }))
    .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Store {
        path: PathBuf,
        config: Config,
    }

    impl Store {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "caos-tree-merge-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            let repo = gix::init_bare(&path).unwrap();
            let config = Config {
                git_dir: path.to_string_lossy().into_owned(),
                repo: repo.into_sync(),
                registry_push_url: String::new(),
                registry_pull_host: String::new(),
                redis_addr: String::new(),
                cache_namespace: String::new(),
            };
            Self { path, config }
        }

        fn tree(&self, files: &[(&str, &str)]) -> String {
            let repo = self.config.repo.to_thread_local();
            let mut entries: Vec<_> = files
                .iter()
                .map(|(name, content)| gix::objs::tree::Entry {
                    mode: gix::objs::tree::EntryKind::Blob.into(),
                    filename: (*name).into(),
                    oid: repo.write_blob(content.as_bytes()).unwrap().detach(),
                })
                .collect();
            entries.sort_by(|a, b| a.filename.cmp(&b.filename));
            repo.write_object(&gix::objs::Tree { entries })
                .unwrap()
                .to_string()
        }

        fn merge(&self, base: &str, ours: &str, theirs: &str) -> serde_json::Value {
            let bytes = merge(
                &self.config,
                &MergeInput {
                    merge_base: base.into(),
                    ours: ours.into(),
                    theirs: theirs.into(),
                },
            )
            .unwrap_or_else(|error| panic!("{}", error.message));
            serde_json::from_slice(&bytes).unwrap()
        }
    }

    impl Drop for Store {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.path).unwrap();
        }
    }

    #[test]
    fn merges_only_trees_without_commits_index_or_worktree() {
        let store = Store::new();
        let base = store.tree(&[("file", "original\n")]);
        let ours = store.tree(&[("file", "original\n"), ("ours", "new\n")]);
        let theirs = store.tree(&[("file", "changed\n")]);
        let result = store.merge(&base, &ours, &theirs);
        let expected = store.tree(&[("file", "changed\n"), ("ours", "new\n")]);
        assert_eq!(result["tree"], expected);
        assert_eq!(result["conflicts"], "");
        assert!(!store.path.join("index").exists());
        assert!(!store.path.join("file").exists());
        assert!(Path::new(&store.config.git_dir).join("objects").is_dir());
    }

    #[test]
    fn returns_native_text_and_delete_modify_conflicts() {
        let store = Store::new();
        let base = store.tree(&[("file", "original\n")]);
        let ours = store.tree(&[("file", "ours\n")]);
        let theirs = store.tree(&[("file", "theirs\n")]);
        let result = store.merge(&base, &ours, &theirs);
        let report = result["conflicts"].as_str().unwrap();
        assert!(report.contains("1\tfile"));
        assert!(report.contains("2\tfile"));
        assert!(report.contains("3\tfile"));
        assert!(report.contains("CONFLICT (content)"));
        let repo = store.config.repo.to_thread_local();
        let tree = repo
            .find_object(
                gix::ObjectId::from_hex(result["tree"].as_str().unwrap().as_bytes()).unwrap(),
            )
            .unwrap()
            .into_tree();
        let entry = tree.find_entry("file").unwrap();
        let blob = repo.find_object(entry.oid().to_owned()).unwrap();
        assert!(String::from_utf8_lossy(&blob.data).contains("<<<<<<<"));

        let deleted = store.tree(&[]);
        let result = store.merge(&base, &deleted, &theirs);
        let report = result["conflicts"].as_str().unwrap();
        assert!(report.contains("CONFLICT (modify/delete)"));
        assert!(report.contains("1\tfile") && report.contains("3\tfile"));
        assert!(!report.contains("2\tfile"));
    }

    #[test]
    fn rejects_revisions_missing_objects_and_wrong_types() {
        let store = Store::new();
        let tree = store.tree(&[]);
        let repo = store.config.repo.to_thread_local();
        let blob = repo.write_blob(b"not a tree").unwrap().to_string();
        for bad in ["HEAD".to_string(), "f".repeat(40), blob] {
            let error = merge(
                &store.config,
                &MergeInput {
                    merge_base: bad,
                    ours: tree.clone(),
                    theirs: tree.clone(),
                },
            )
            .unwrap_err();
            assert_eq!(error.status, 400);
        }
    }
}
