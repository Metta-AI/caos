//! The conversation client has its own harness checkout. Attached code never
//! replaces that harness, and launcher state never enters a source tree.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use caos::GitTransport;
use conversation_protocol::v3::{GitStore, Oid};

use super::args::Args;

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("starting git: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    String::from_utf8(output.stdout)
        .map(|s| s.trim_end().to_string())
        .map_err(|e| e.to_string())
}

pub(super) use caos_cli::source_trees::import_local_commit;

fn data_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or("set XDG_DATA_HOME or HOME for the conversation client")?;
    Ok(base.join("caos"))
}

pub(super) fn prepare(args: &mut Args) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let checkout = GitTransport::discover(&cwd)
        .ok()
        .map(|t| t.work_dir().to_path_buf());
    let source = args.harness.as_ref().map(PathBuf::from)
        .or_else(|| std::env::var_os("CAOS_HARNESS_SOURCE").map(PathBuf::from))
        .or_else(|| checkout.clone())
        .ok_or("this build has no bundled harness; use the packaged caos or pass --harness <caos checkout>")?
        .canonicalize().map_err(|e| format!("locating the harness: {e}"))?;
    let server = args
        .server
        .clone()
        .or_else(|| {
            checkout
                .as_ref()
                .and_then(|repo| git(repo, &["remote", "get-url", "caos"]).ok())
        })
        .unwrap_or_else(|| "http://localhost:9090".into());
    if server.is_empty() || server.starts_with('-') || server.chars().any(char::is_control) {
        return Err("invalid --server URL".into());
    }
    let data = data_dir()?;
    fs::create_dir_all(&data).map_err(|e| e.to_string())?;
    let data = data.canonicalize().map_err(|e| e.to_string())?;
    // Reuse the launching checkout's existing store without copying credentials.
    let secrets = checkout
        .as_ref()
        .map(|repo| repo.join(caos::SECRETS_DIR))
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| data.join("secrets"));
    let client = create_client(&source, &data, &server, &secrets, checkout.as_deref())?;
    if let Some(file) = &mut args.turn.system_file {
        *file = cwd.join(&*file).to_string_lossy().into_owned();
    }
    if let Some(file) = args.turn.system_file.take() {
        args.turn.system =
            Some(fs::read_to_string(&file).map_err(|e| format!("reading {file}: {e}"))?);
    }
    let mut seed = conversation_protocol::v3::tree::TreeBuilder::from(None);
    if let Some(name) = &args.import {
        conversation_protocol::v3::paths::validate_source_tree_name(name)?;
        if let Some(checkout) = &checkout {
            let transport = GitTransport::discover(&client)?;
            let caos_cli::source_trees::ImportedContent { commit, metadata } =
                caos_cli::source_trees::prepare_import(
                    &transport,
                    name,
                    checkout.to_str().ok_or("checkout path must be UTF-8")?,
                    args.turn.base.as_deref(),
                )?;
            seed.put_oid(
                name,
                conversation_protocol::v3::Mode::Commit,
                commit.clone(),
            );
            if let Some((path, bytes)) = metadata {
                seed.put(&path, conversation_protocol::v3::Mode::Blob, bytes);
            }
            args.turn.base = Some(commit.to_string());
            if args.from_commit.is_some() {
                args.from_commit = Some(commit.to_string());
            }
        } else {
            return Err("--import requires a Git checkout".into());
        }
    }
    args.turn.initial_content = Some(
        seed.build(&mut GitStore::open(&client, Some(&server))?)?
            .to_string(),
    );
    Ok(client)
}

struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy_entry(source: &Path, dest: &Path) -> Result<(), String> {
    let meta =
        fs::symlink_metadata(source).map_err(|e| format!("reading {}: {e}", source.display()))?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source).map_err(|e| e.to_string())?, dest)
            .map_err(|e| e.to_string())?;
    } else if meta.is_dir() {
        fs::create_dir_all(dest).map_err(|e| e.to_string())?;
        for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_entry(&entry.path(), &dest.join(entry.file_name()))?;
        }
    } else if meta.is_file() {
        fs::copy(source, dest).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn hash_key(dir: &Path, value: &serde_json::Value) -> Result<String, String> {
    use std::io::Write;
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .ok_or("hash input missing")?
        .write_all(value.to_string().as_bytes())
        .map_err(|e| e.to_string())?;
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn create_harness(source: &Path, data: &Path) -> Result<PathBuf, String> {
    let harnesses = data.join("harnesses");
    fs::create_dir_all(&harnesses).map_err(|e| e.to_string())?;
    let immutable = source.starts_with("/nix/store");
    let pinned = if immutable {
        Some(harnesses.join(hash_key(data, &serde_json::json!(["harness-v1", source]))?))
    } else {
        None
    };
    if let Some(path) = &pinned {
        if path.exists() {
            return Ok(path.clone());
        }
    }
    let staging = Staging(harnesses.join(format!(".new-{}", caos::fresh_entropy()?)));
    fs::create_dir(&staging.0).map_err(|e| e.to_string())?;
    let tracked = if source.join(".git").exists() {
        git(source, &["ls-files", "-z"])?
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    } else {
        fs::read_dir(source)
            .map_err(|e| e.to_string())?
            .map(|e| {
                e.map(|e| PathBuf::from(e.file_name()))
                    .map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    for file in tracked {
        let first = file
            .components()
            .next()
            .map(|part| part.as_os_str().to_string_lossy())
            .unwrap_or_default();
        if matches!(
            first.as_ref(),
            ".git" | ".caos-secrets" | ".caos-data" | ".task" | ".direnv" | "target"
        ) || first.starts_with("result")
        {
            continue;
        }
        if source.join(&file).symlink_metadata().is_ok() {
            copy_entry(&source.join(&file), &staging.0.join(file))?;
        }
    }
    git(&staging.0, &["init", "--quiet", "-b", "main"])?;
    git(&staging.0, &["config", "user.name", "caos"])?;
    git(&staging.0, &["config", "user.email", "caos@localhost"])?;
    git(&staging.0, &["config", "gc.auto", "0"])?;
    git(&staging.0, &["add", "-A"])?;
    let tree = git(&staging.0, &["write-tree"])?;
    let destination = pinned.unwrap_or_else(|| harnesses.join(tree));
    if destination.exists() {
        return Ok(destination);
    }
    git(
        &staging.0,
        &[
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--quiet",
            "-m",
            "conversation harness",
        ],
    )?;
    match fs::rename(&staging.0, &destination) {
        Ok(()) => Ok(destination),
        Err(_) if destination.exists() => Ok(destination),
        Err(error) => Err(format!("installing harness: {error}")),
    }
}

fn create_client(
    source: &Path,
    data: &Path,
    server: &str,
    secrets: &Path,
    checkout: Option<&Path>,
) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let harness = create_harness(source, data)?;
    let clients = data.join("clients");
    fs::create_dir_all(&clients).map_err(|e| e.to_string())?;
    // Structured policy avoids ambiguous keys when paths contain newlines.
    let policy = hash_key(data, &serde_json::json!([server, secrets, checkout]))?;
    let artifact = harness
        .file_name()
        .ok_or("harness has no name")?
        .to_string_lossy();
    let destination = clients.join(format!("{artifact}-{policy}"));
    if destination.exists() {
        git(
            &destination,
            &[
                "config",
                "caos.checkout-settings",
                &data.join("checkouts.gitconfig").to_string_lossy(),
            ],
        )?;
        return Ok(destination);
    }
    let staging = Staging(clients.join(format!(".new-{}", caos::fresh_entropy()?)));
    git(
        data,
        &[
            "clone",
            "--quiet",
            "--local",
            "--no-hardlinks",
            &harness.to_string_lossy(),
            &staging.0.to_string_lossy(),
        ],
    )?;
    git(&staging.0, &["remote", "remove", "origin"])?;
    git(&staging.0, &["config", "gc.auto", "0"])?;
    git(&staging.0, &["remote", "add", "caos", server])?;
    git(&staging.0, &["config", "caos.launcher", "true"])?;
    git(
        &staging.0,
        &[
            "config",
            "caos.checkout-settings",
            &data.join("checkouts.gitconfig").to_string_lossy(),
        ],
    )?;
    if let Some(checkout) = checkout {
        git(
            &staging.0,
            &["config", "caos.checkout", &checkout.to_string_lossy()],
        )?;
    }
    fs::write(staging.0.join(".git/info/exclude"), "/.caos-secrets\n")
        .map_err(|e| e.to_string())?;
    if !secrets.exists() {
        fs::create_dir_all(secrets).map_err(|e| e.to_string())?;
        fs::set_permissions(secrets, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    std::os::unix::fs::symlink(secrets, staging.0.join(caos::SECRETS_DIR))
        .map_err(|e| e.to_string())?;
    match fs::rename(&staging.0, &destination) {
        Ok(()) => Ok(destination),
        Err(_) if destination.exists() => Ok(destination),
        Err(error) => Err(format!("installing client store: {error}")),
    }
}

/// Local checkout preferences are keyed by conversation and gitlink path, never
/// inferred from a publishing destination or included in conversation content.
fn checkout_key(client: &Path, conversation: &str, source: &str) -> Result<String, String> {
    Ok(format!(
        "caos.checkout-{}",
        hash_key(
            client,
            &serde_json::json!([
                git(client, &["remote", "get-url", "caos"]).ok(),
                conversation,
                source
            ])
        )?
    ))
}

pub(super) fn remember_checkout(
    client: &Path,
    conversation: &str,
    source: &str,
    destination: &Path,
) -> Result<(), String> {
    git(
        client,
        &[
            "config",
            "--file",
            &git(client, &["config", "--get", "caos.checkout-settings"])?,
            &checkout_key(client, conversation, source)?,
            &destination.to_string_lossy(),
        ],
    )?;
    Ok(())
}

/// Publication preferences stay beside local checkout preferences, outside CAOS.
pub(super) fn checkout_for(
    client: &Path,
    conversation: &str,
    source: &str,
    head: &str,
) -> Result<PathBuf, String> {
    let value = git(
        client,
        &[
            "config",
            "--file",
            &git(client, &["config", "--get", "caos.checkout-settings"])?,
            "--null",
            "--get",
            &checkout_key(client, conversation, source)?,
        ],
    )
    .map_err(|_| {
        format!(
            "choose a local destination with /checkout {} <directory>",
            shell_words::quote(source)
        )
    })?;
    let checkout = PathBuf::from(value.strip_suffix('\0').ok_or("invalid checkout config")?);
    import_local_commit(
        client,
        &checkout,
        &Oid::parse(head, "source tree checkout")?,
    )?;
    Ok(checkout)
}

/// Resolve user-entered paths relative to where the TUI was launched.
pub(super) fn local_path(client: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return path;
    }
    let launch = git(client, &["config", "--null", "--get", "caos.checkout"])
        .ok()
        .and_then(|value| value.strip_suffix('\0').map(PathBuf::from));
    launch.unwrap_or_else(|| client.into()).join(path)
}

pub(super) fn prepare_checkout(destination: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(destination).map_err(|e| format!("creating checkout: {e}"))?;
    let destination = destination.canonicalize().map_err(|e| e.to_string())?;
    if fs::read_dir(&destination)
        .map_err(|e| e.to_string())?
        .next()
        .is_none()
    {
        git(&destination, &["init", "--quiet"])?;
    }
    let root = git(&destination, &["rev-parse", "--show-toplevel"])?;
    if Path::new(&root) != destination {
        return Err("choose the root of a Git checkout or an empty directory".into());
    }
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checkout_import_completes_partial_history_and_keeps_local_edits() {
        let root = Staging(std::env::temp_dir().join(format!(
            "launcher-import-{}",
            caos::fresh_entropy().unwrap()
        )));
        let origin = root.0.join("origin");
        fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "--quiet", "-b", "main"]).unwrap();
        git(&origin, &["config", "user.name", "test"]).unwrap();
        git(&origin, &["config", "user.email", "test@example.invalid"]).unwrap();
        git(&origin, &["config", "uploadpack.allowFilter", "true"]).unwrap();
        fs::write(origin.join("old.txt"), "historical blob\n").unwrap();
        git(&origin, &["add", "old.txt"]).unwrap();
        git(
            &origin,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "first"],
        )
        .unwrap();
        let old_blob = Oid::parse(
            &git(&origin, &["rev-parse", "HEAD:old.txt"]).unwrap(),
            "old blob",
        )
        .unwrap();
        git(&origin, &["rm", "old.txt"]).unwrap();
        git(
            &origin,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "remove old file",
            ],
        )
        .unwrap();
        let checkout = root.0.join("checkout");
        git(
            &root.0,
            &[
                "clone",
                "--quiet",
                "--filter=blob:none",
                &format!("file://{}", origin.display()),
                &checkout.to_string_lossy(),
            ],
        )
        .unwrap();
        git(&checkout, &["config", "user.name", "test"]).unwrap();
        git(&checkout, &["config", "user.email", "test@example.invalid"]).unwrap();
        fs::write(checkout.join("new.txt"), "unpushed commit\n").unwrap();
        git(&checkout, &["add", "new.txt"]).unwrap();
        git(
            &checkout,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "local change",
            ],
        )
        .unwrap();
        let head =
            Oid::parse(&git(&checkout, &["rev-parse", "HEAD"]).unwrap(), "checkout").unwrap();
        fs::write(checkout.join("new.txt"), "uncommitted edit\n").unwrap();
        let status = git(&checkout, &["status", "--porcelain"]).unwrap();
        assert!(!GitStore::open(&checkout, None)
            .unwrap()
            .has_local(&old_blob)
            .unwrap());
        let client = root.0.join("client");
        fs::create_dir(&client).unwrap();
        git(&client, &["init", "--quiet", "-b", "main"]).unwrap();
        // Checkout exports from a partial client into the user's checkout.
        git(
            &checkout,
            &[
                "config",
                "caos.checkout-settings",
                &root.0.join("checkouts.gitconfig").to_string_lossy(),
            ],
        )
        .unwrap();
        remember_checkout(&checkout, "conversation", "feature/dirty", &client).unwrap();
        assert_eq!(
            checkout_for(&checkout, "conversation", "feature/dirty", head.as_str()).unwrap(),
            client
        );
        import_local_commit(&checkout, &client, &head).unwrap();
        // The destination is self-contained even after the source disappears.
        fs::remove_dir_all(&origin).unwrap();
        assert_eq!(git(&checkout, &["status", "--porcelain"]).unwrap(), status);
        assert_eq!(
            git(&checkout, &["rev-parse", "HEAD"]).unwrap(),
            head.as_str()
        );
        fs::remove_dir_all(&checkout).unwrap();
        git(
            &client,
            &["rev-list", "--objects", "--missing=error", head.as_str()],
        )
        .unwrap();
        assert_eq!(
            git(&client, &["show", &format!("{head}:new.txt")]).unwrap(),
            "unpushed commit"
        );
        assert_eq!(
            git(&client, &["cat-file", "blob", old_blob.as_str()]).unwrap(),
            "historical blob"
        );
        assert_eq!(git(&client, &["for-each-ref"]).unwrap(), "");
        assert!(!client.join(".git/FETCH_HEAD").exists());
    }

    #[test]
    fn launcher_caches_only_matching_source_and_local_policy() {
        let root =
            std::env::temp_dir().join(format!("launcher-{}", caos::fresh_entropy().unwrap()));
        let source = root.join("source");
        fs::create_dir_all(source.join("std/llm-step")).unwrap();
        fs::write(source.join("DEPS"), "./std/llm-step llm-step\n").unwrap();
        fs::write(source.join("std/llm-step/.caos-expr"), "fixture\n").unwrap();
        fs::create_dir_all(source.join(".caos-secrets")).unwrap();
        fs::write(source.join(".caos-secrets/never-copy"), "fixture only").unwrap();
        let data = root.join("data");
        let secrets = data.join("secrets");
        let first = create_client(&source, &data, "http://localhost:9090", &secrets, None).unwrap();
        assert_eq!(
            create_client(&source, &data, "http://localhost:9090", &secrets, None).unwrap(),
            first
        );
        assert!(!git(&first, &["ls-files"])
            .unwrap()
            .contains(".caos-secrets"));
        assert!(!secrets.join("never-copy").exists());
        assert!(first.join(caos::SECRETS_DIR).is_dir());
        assert_eq!(git(&first, &["status", "--porcelain"]).unwrap(), "");
        let second =
            create_client(&source, &data, "http://localhost:9091", &secrets, None).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read_dir(data.join("harnesses")).unwrap().count(), 1);
        assert!(!fs::read_dir(data.join("harnesses"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join(caos::SECRETS_DIR)
            .exists());
        fs::write(source.join("DEPS"), "changed\n").unwrap();
        let third = create_client(&source, &data, "http://localhost:9090", &secrets, None).unwrap();
        assert_ne!(first, third);
        assert_eq!(
            fs::read_to_string(first.join("DEPS")).unwrap(),
            "./std/llm-step llm-step\n"
        );
        let error =
            checkout_for(&first, "conversation", "feature/dirty", &"a".repeat(40)).unwrap_err();
        assert!(error.contains("choose a local destination"));
        remember_checkout(&first, "conversation", "feature/dirty", &first).unwrap();
        let head = git(&first, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(
            checkout_for(&third, "conversation", "feature/dirty", &head).unwrap(),
            first
        );
        assert!(checkout_for(&third, "other", "feature/dirty", &head).is_err());
        assert_eq!(git(&first, &["status", "--porcelain"]).unwrap(), "");
        fs::remove_dir_all(root).unwrap();
    }
}
