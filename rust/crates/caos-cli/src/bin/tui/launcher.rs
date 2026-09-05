//! The conversation client has its own harness checkout. Attached code never
//! replaces that harness, and launcher state never enters a workspace tree.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use caos::GitTransport;
use caos_cli::InitialWorkspace;
use conversation_protocol::v3::{GitStore, Oid, WorkspaceBase, WorkspaceConfig};

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

/// Import from the user's trusted checkout, including its unpushed commits.
/// upload-pack otherwise forbids fetching promised objects from a partial clone.
/// Keep the override on this local import, never on arbitrary repository fetches.
pub(super) fn import_checkout_commit(
    checkout: &Path,
    client: &Path,
    commit: &Oid,
) -> Result<(), String> {
    if GitStore::open(client, None)?.has_local(commit)? {
        return Ok(());
    }
    let checkout = checkout.canonicalize().map_err(|e| e.to_string())?;
    let output = Command::new("git")
        .current_dir(client)
        .env("GIT_NO_LAZY_FETCH", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "fetch.negotiationAlgorithm=noop",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
        ])
        .arg(&checkout)
        .arg(commit.as_str())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("starting checkout import: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "importing checkout history: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

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
        .or_else(|| checkout.as_ref().filter(|path| path.join("std/llm-step/.caos-expr").exists()).cloned())
        .ok_or("this build has no bundled harness; use the packaged caos or pass --harness <caos checkout>")?
        .canonicalize().map_err(|e| format!("locating the harness: {e}"))?;
    if !source.join("DEPS").is_file() || !source.join("std/llm-step/.caos-expr").is_file() {
        return Err("the harness needs its root DEPS and std/llm-step entry".into());
    }
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
    let mut seeds = BTreeMap::new();
    if !args.empty {
        if let Some(checkout) = &checkout {
            let rev = args.turn.base.as_deref().unwrap_or("HEAD");
            let commit = git(
                checkout,
                &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
            )?;
            let oid = Oid::parse(&commit, "initial checkout")?;
            import_checkout_commit(checkout, &client, &oid)?;
            let repository = git(checkout, &["remote", "get-url", "origin"])
                .unwrap_or_else(|_| checkout.to_string_lossy().into_owned());
            let repository =
                if Path::new(&repository).exists() || checkout.join(&repository).exists() {
                    checkout
                        .join(&repository)
                        .canonicalize()
                        .map_err(|e| e.to_string())?
                        .to_string_lossy()
                        .into_owned()
                } else {
                    repository
                };
            let mut config = WorkspaceConfig {
                repository: Some(repository),
                ..Default::default()
            };
            // A local remote-tracking tip is enough to identify the integrated
            // base without fetching during startup. Update stack refreshes it.
            let base_ref = git(
                checkout,
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
            )
            .or_else(|_| git(checkout, &["symbolic-ref", "--quiet", "HEAD"]))
            .ok();
            if let Some(base_ref) = base_ref {
                let name = base_ref
                    .strip_prefix("refs/remotes/origin/")
                    .or_else(|| base_ref.strip_prefix("refs/heads/"))
                    .unwrap_or(&base_ref);
                if let Ok(base) = git(checkout, &["merge-base", &commit, &base_ref]) {
                    config.base = Some(WorkspaceBase::Branch {
                        name: name.into(),
                        commit: Oid::parse(&base, "checkout base")?,
                    });
                }
            }
            config.validate()?;
            seeds.insert(
                "main".into(),
                InitialWorkspace {
                    commit: commit.clone(),
                    config,
                },
            );
            args.turn.base = Some(commit.clone());
            if args.from_commit.is_some() {
                args.from_commit = Some(commit);
            }
        } else if args.turn.base.is_some() {
            return Err(
                "--base and --from require a checkout; use --empty and attach a repository".into(),
            );
        }
    }
    args.turn.initial_workspaces = Some(seeds);
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

fn create_client(
    source: &Path,
    data: &Path,
    server: &str,
    secrets: &Path,
    checkout: Option<&Path>,
) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let clients = data.join("clients");
    fs::create_dir_all(&clients).map_err(|e| e.to_string())?;
    let staging = Staging(clients.join(format!(".new-{}", caos::fresh_entropy()?)));
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
    // The key includes local policy too: never reuse another checkout's secret
    // store or retarget an existing client's server.
    fs::write(
        staging.0.join(".git/launcher-key"),
        format!(
            "{server}\n{}\n{}",
            secrets.display(),
            checkout.map(|p| p.to_string_lossy()).unwrap_or_default()
        ),
    )
    .map_err(|e| e.to_string())?;
    let policy = git(&staging.0, &["hash-object", ".git/launcher-key"])?;
    let destination = clients.join(format!("{tree}-{policy}"));
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
    git(&staging.0, &["remote", "add", "caos", server])?;
    git(&staging.0, &["config", "caos.launcher", "true"])?;
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

/// Existing checkout commands remain tied to matching user code, never to the
/// harness. Importing objects is harmless; checking out/committing stays explicit.
pub(super) fn checkout_for(
    client: &Path,
    config: &WorkspaceConfig,
    head: &str,
) -> Result<PathBuf, String> {
    if git(client, &["config", "--get", "caos.launcher"])
        .ok()
        .as_deref()
        != Some("true")
    {
        return Ok(client.into());
    }
    let checkout = PathBuf::from(git(client, &["config", "--get", "caos.checkout"]).map_err(
        |_| "this client has no local checkout; open caos from a checkout to use checkout commands",
    )?);
    let repository = git(&checkout, &["remote", "get-url", "origin"])
        .unwrap_or_else(|_| checkout.to_string_lossy().into_owned());
    if config.repository.as_deref().is_none_or(|repo| {
        caos_cli::normalize_repository_identity(repo).ok()
            != caos_cli::normalize_repository_identity(&repository).ok()
    }) {
        return Err("selected workspace belongs to another repository; open caos from a matching checkout to use checkout commands".into());
    }
    GitStore::open(&checkout, Some(&client.to_string_lossy()))?
        .ensure_local(&Oid::parse(head, "workspace checkout")?)?;
    Ok(checkout)
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
        import_checkout_commit(&checkout, &client, &head).unwrap();
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
        fs::write(source.join("DEPS"), "changed\n").unwrap();
        let third = create_client(&source, &data, "http://localhost:9090", &secrets, None).unwrap();
        assert_ne!(first, third);
        assert_eq!(
            fs::read_to_string(first.join("DEPS")).unwrap(),
            "./std/llm-step llm-step\n"
        );
        let error = checkout_for(&first, &WorkspaceConfig::default(), &"a".repeat(40)).unwrap_err();
        assert!(error.contains("no local checkout"));
        fs::remove_dir_all(root).unwrap();
    }
}
