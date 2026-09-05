use std::cell::{Cell, RefCell};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::write::ZlibEncoder;
use flate2::Compression;

use super::oid::{object_id, ObjectKind, Oid};
use super::reconcile::{CodeOps, MergeOutcome};
use super::tree::{
    encode_commit_bytes, encode_tree_bytes, is_canonical_tree_order, parse_commit_bytes,
    parse_tree_bytes, CommitInfo, ObjectStore, Signature, StoreError, TreeEntry,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUpdate {
    pub refname: String,
    pub expected: Option<Oid>,
    pub new: Option<Oid>,
}

pub struct GitStore {
    dir: PathBuf,
    git_dir: PathBuf,
    remote: Option<String>,
    remote_tip: RefCell<Option<Oid>>,
    batch: RefCell<Option<BatchReader>>,
    batch_dirty: Cell<bool>,
    #[cfg(test)]
    command_count: Cell<usize>,
}

static NEXT_OBJECT_TEMP: AtomicU64 = AtomicU64::new(1);

fn quote_config_value(value: &str) -> Result<String, String> {
    if value.contains('\0') {
        return Err("Git remote URL contains NUL".to_string());
    }
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '\"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\r' => quoted.push_str("\\r"),
            character => quoted.push(character),
        }
    }
    quoted.push('\"');
    Ok(quoted)
}

fn fetch_arguments(tip: Option<&Oid>) -> Vec<String> {
    let mut arguments = vec!["-c".to_string()];
    if let Some(tip) = tip {
        arguments.extend([
            "fetch.negotiationAlgorithm=default".to_string(),
            "fetch".to_string(),
            "--quiet".to_string(),
            "--no-tags".to_string(),
            "--no-write-fetch-head".to_string(),
            format!("--negotiation-tip={tip}"),
        ]);
    } else {
        arguments.extend([
            "fetch.negotiationAlgorithm=noop".to_string(),
            "fetch".to_string(),
            "--quiet".to_string(),
            "--no-tags".to_string(),
            "--no-write-fetch-head".to_string(),
        ]);
    }
    arguments
}

fn resolve_common_dir(git_dir: &Path) -> Result<PathBuf, String> {
    let path = git_dir.join("commondir");
    let common = match fs::read_to_string(&path) {
        Ok(common) => common,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(git_dir.to_path_buf())
        }
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    };
    let common = Path::new(common.trim());
    let common = if common.is_absolute() {
        common.to_path_buf()
    } else {
        git_dir.join(common)
    };
    fs::canonicalize(&common).map_err(|error| {
        format!(
            "resolving Git common directory {}: {error}",
            common.display()
        )
    })
}

struct BatchReader {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

enum BatchResult {
    Object { kind: String, bytes: Vec<u8> },
    Missing,
    Dead(String),
}

impl GitStore {
    pub fn scratch(name: &str, remote_url: &str) -> Result<GitStore, String> {
        let dir = PathBuf::from(format!("/tmp/{name}"));
        if let Err(error) = fs::remove_dir_all(&dir) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(format!("clearing {}: {error}", dir.display()));
            }
        }
        fs::create_dir_all(dir.join("objects"))
            .map_err(|error| format!("creating scratch object directory: {error}"))?;
        fs::create_dir_all(dir.join("refs"))
            .map_err(|error| format!("creating scratch refs directory: {error}"))?;
        fs::write(dir.join("HEAD"), b"ref: refs/heads/master\n")
            .map_err(|error| format!("writing scratch HEAD: {error}"))?;
        let config = format!(
            "[core]\n\trepositoryformatversion = 0\n\tbare = true\n\
             [remote \"origin\"]\n\turl = {}\n\
             [protocol]\n\tversion = 2\n\
             [gc]\n\tauto = 0\n\
             [receive]\n\tautogc = false\n\
             [maintenance \"geometric-repack\"]\n\tenabled = false\n",
            quote_config_value(remote_url)?
        );
        fs::write(dir.join("config"), config)
            .map_err(|error| format!("writing scratch config: {error}"))?;
        GitStore::open(&dir, Some("origin"))
    }

    pub fn open(dir: &Path, remote: Option<&str>) -> Result<GitStore, String> {
        let dir = fs::canonicalize(dir)
            .map_err(|error| format!("resolving Git repository {}: {error}", dir.display()))?;
        let mut store = GitStore {
            git_dir: dir.clone(),
            dir,
            remote: remote.map(str::to_string),
            remote_tip: RefCell::new(None),
            batch: RefCell::new(None),
            batch_dirty: Cell::new(false),
            #[cfg(test)]
            command_count: Cell::new(0),
        };
        if store.dir.join("HEAD").is_file()
            && store.dir.join("objects").is_dir()
            && store.dir.join("refs").is_dir()
        {
            return Ok(store);
        }
        let output = store.output(&["rev-parse", "--absolute-git-dir"])?;
        if !output.status.success() {
            return Err(store.output_error("git rev-parse", &output));
        }
        let git_dir = String::from_utf8(output.stdout)
            .map_err(|_| "git rev-parse returned a non-UTF-8 Git directory".to_string())?;
        let git_dir = fs::canonicalize(git_dir.trim()).map_err(|error| {
            format!(
                "resolving Git directory {}: {error}",
                Path::new(git_dir.trim()).display()
            )
        })?;
        store.git_dir = resolve_common_dir(&git_dir)?;
        Ok(store)
    }

    pub fn read_ref(&self, refname: &str) -> Result<Option<Oid>, String> {
        let Some(remote) = &self.remote else {
            return self.read_local_ref(refname);
        };
        let output = self.output(&["ls-remote", "--refs", remote, refname])?;
        if !output.status.success() {
            return Err(self.output_error("git ls-remote", &output));
        }
        let stdout = String::from_utf8(output.stdout)
            .map_err(|_| "git ls-remote returned non-UTF-8 output".to_string())?;
        let mut matches = Vec::new();
        for line in stdout.lines() {
            let mut fields = line.split_whitespace();
            let oid = fields
                .next()
                .ok_or_else(|| "git ls-remote returned a malformed line".to_string())?;
            let name = fields
                .next()
                .ok_or_else(|| "git ls-remote returned a malformed line".to_string())?;
            if fields.next().is_some() {
                return Err("git ls-remote returned a malformed line".to_string());
            }
            if name == refname {
                matches.push(Oid::parse(oid, "remote ref oid")?);
            }
        }
        Ok(matches.pop())
    }

    pub fn fetch_ref(&self, refname: &str) -> Result<Option<Oid>, String> {
        let remote = self
            .remote
            .as_deref()
            .ok_or_else(|| "no remote".to_string())?;
        let refspec = format!("{refname}:{refname}");
        let tip = self
            .read_local_ref(refname)?
            .or_else(|| self.remote_tip.borrow().clone());
        let mut arguments = fetch_arguments(tip.as_ref());
        arguments.extend([remote.to_string(), refspec]);
        let argument_refs: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let output = self.output(&argument_refs)?;
        if output.status.success() {
            let head = self.read_local_ref(refname)?;
            if let Some(head) = &head {
                *self.remote_tip.borrow_mut() = Some(head.clone());
            }
            return Ok(head);
        }
        let probe = self.output(&["ls-remote", "--exit-code", "--refs", remote, refname])?;
        if probe.status.code() == Some(2) {
            Ok(None)
        } else {
            Err(self.output_error("git fetch", &output))
        }
    }

    pub fn fetch_object(&self, oid: &Oid) -> Result<(), String> {
        self.fetch_object_with_tip(oid, None)
    }

    pub fn fetch_object_with_tip(&self, oid: &Oid, tip: Option<&Oid>) -> Result<(), String> {
        let remote = self
            .remote
            .as_deref()
            .ok_or_else(|| "no remote".to_string())?;
        let mut arguments = fetch_arguments(tip);
        arguments.extend([remote.to_string(), oid.to_string()]);
        let argument_refs: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let output = self.output(&argument_refs)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(self.output_error("git fetch", &output))
        }
    }

    pub fn has_local(&self, oid: &Oid) -> Result<bool, String> {
        let object = format!("{oid}^{{object}}");
        let output = self
            .command()
            .env("GIT_NO_LAZY_FETCH", "1")
            .args(["cat-file", "-e", &object])
            .output()
            .map_err(|error| format!("failed to run git: {error}"))?;
        if output.status.success() {
            return Ok(true);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if matches!(output.status.code(), Some(1)) || stderr.contains("Not a valid object name") {
            Ok(false)
        } else {
            Err(self.output_error("git cat-file", &output))
        }
    }

    pub fn ensure_local(&self, oid: &Oid) -> Result<(), String> {
        if !self.has_local(oid)? {
            self.fetch_object(oid)?;
        }
        Ok(())
    }

    pub fn ensure_local_with_tip(&self, oid: &Oid, tip: &Oid) -> Result<(), String> {
        if !self.has_local(oid)? {
            self.fetch_object_with_tip(oid, Some(tip))?;
        }
        Ok(())
    }

    pub fn push(&self, updates: &[RefUpdate]) -> Result<(), String> {
        if updates.is_empty() {
            return Ok(());
        }
        let remote = self
            .remote
            .as_deref()
            .ok_or_else(|| "no remote".to_string())?;
        let mut arguments = vec!["push".to_string(), "--quiet".to_string()];
        if updates.len() > 1 {
            arguments.push("--atomic".to_string());
        }
        for update in updates {
            let expected = update.expected.as_ref().map(Oid::as_str).unwrap_or("");
            arguments.push(format!("--force-with-lease={}:{expected}", update.refname));
        }
        arguments.push(remote.to_string());
        for update in updates {
            let new = update.new.as_ref().map(Oid::as_str).unwrap_or("");
            arguments.push(format!("{new}:{}", update.refname));
        }
        let argument_refs: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let output = self.output(&argument_refs)?;
        if output.status.success() {
            if let Err(error) = self.update_local_refs(updates) {
                eprintln!(
                    "conversation-protocol: warning: push succeeded but local refs were not advanced: {error}"
                );
            }
            if let Some(tip) = self.pushed_remote_tip(updates) {
                *self.remote_tip.borrow_mut() = Some(tip);
            }
            Ok(())
        } else {
            Err(self.output_error("git push", &output))
        }
    }

    pub fn git_version(&self) -> Result<String, String> {
        let output = self.output(&["version"])?;
        if !output.status.success() {
            return Err(self.output_error("git version", &output));
        }
        let stdout = String::from_utf8(output.stdout)
            .map_err(|_| "git version returned non-UTF-8 output".to_string())?;
        stdout
            .trim()
            .strip_prefix("git version ")
            .map(str::to_string)
            .ok_or_else(|| format!("unexpected git version output {stdout:?}"))
    }

    fn command(&self) -> Command {
        #[cfg(test)]
        self.command_count.set(self.command_count.get() + 1);
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.dir)
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    fn output(&self, arguments: &[&str]) -> Result<Output, String> {
        self.command()
            .args(arguments)
            .output()
            .map_err(|error| format!("failed to run git: {error}"))
    }

    fn output_error(&self, action: &str, output: &Output) -> String {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            format!("{action} failed with {}", output.status)
        } else {
            stderr
        }
    }

    fn read_local_ref(&self, refname: &str) -> Result<Option<Oid>, String> {
        if !self.git_dir.join("reftable").exists() {
            return self.read_files_ref(refname, 0);
        }
        self.read_local_ref_with_git(refname)
    }

    fn read_files_ref(&self, refname: &str, depth: usize) -> Result<Option<Oid>, String> {
        if depth > 8 {
            return Err(format!("symbolic ref {refname:?} is too deep"));
        }
        let path = self.loose_ref_path(refname)?;
        match fs::read_to_string(&path) {
            Ok(value) => {
                let value = value.trim();
                if let Some(target) = value.strip_prefix("ref: ") {
                    return self.read_files_ref(target, depth + 1);
                }
                return Oid::parse(value, "local ref oid").map(Some);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("reading {}: {error}", path.display())),
        }
        let packed_path = self.git_dir.join("packed-refs");
        let packed = match fs::read_to_string(&packed_path) {
            Ok(packed) => packed,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("reading {}: {error}", packed_path.display())),
        };
        for line in packed.lines() {
            if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let Some((oid, name)) = line.split_once(' ') else {
                return Err(format!("malformed packed ref in {}", packed_path.display()));
            };
            if name == refname {
                return Oid::parse(oid, "packed local ref oid").map(Some);
            }
        }
        Ok(None)
    }

    fn read_local_ref_with_git(&self, refname: &str) -> Result<Option<Oid>, String> {
        let output = self.output(&["rev-parse", "--verify", "--quiet", refname])?;
        match output.status.code() {
            Some(0) => {
                let stdout = String::from_utf8(output.stdout)
                    .map_err(|_| "git rev-parse returned non-UTF-8 output".to_string())?;
                Ok(Some(Oid::parse(stdout.trim(), "local ref oid")?))
            }
            Some(1) => Ok(None),
            _ => Err(self.output_error("git rev-parse", &output)),
        }
    }

    fn update_local_refs(&self, updates: &[RefUpdate]) -> Result<(), String> {
        let updates = updates
            .iter()
            .filter(|update| update.refname.starts_with("refs/caos/"))
            .collect::<Vec<_>>();
        if updates.is_empty() {
            return Ok(());
        }
        let mut child = self
            .command()
            .args(["update-ref", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("running git update-ref: {error}"))?;
        let written = {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| "git update-ref has no standard input".to_string())?;
            updates.iter().try_for_each(|update| match &update.new {
                Some(new) => writeln!(stdin, "update {} {new}", update.refname),
                None => writeln!(stdin, "delete {}", update.refname),
            })
        };
        let output = child
            .wait_with_output()
            .map_err(|error| format!("waiting for git update-ref: {error}"))?;
        written.map_err(|error| format!("writing git update-ref input: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(self.output_error("git update-ref", &output))
        }
    }

    fn pushed_remote_tip(&self, updates: &[RefUpdate]) -> Option<Oid> {
        updates
            .iter()
            .find(|update| {
                update.refname.starts_with("refs/caos/v3/conversations/") && update.new.is_some()
            })
            .and_then(|update| update.new.clone())
            .or_else(|| {
                updates
                    .iter()
                    .filter_map(|update| update.new.as_ref())
                    .find(|oid| self.read_commit(oid).is_ok())
                    .cloned()
            })
    }

    fn loose_ref_path(&self, refname: &str) -> Result<PathBuf, String> {
        if !refname.starts_with("refs/")
            || refname
                .split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
            || refname.contains(['\\', '\0'])
        {
            return Err(format!("unsafe Git ref name {refname:?}"));
        }
        Ok(self.git_dir.join(refname))
    }

    fn spawn_batch(&self) -> Result<BatchReader, StoreError> {
        let mut child = self
            .command()
            .env("GIT_NO_LAZY_FETCH", "1")
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| StoreError::Other(format!("failed to run git cat-file: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| StoreError::Other("git cat-file has no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| StoreError::Other("git cat-file has no stdout".to_string()))?;
        Ok(BatchReader {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    fn reset_batch(&self) {
        if let Some(mut batch) = self.batch.borrow_mut().take() {
            let _ = batch.child.kill();
            let _ = batch.child.wait();
        }
    }

    fn read_batch_once(&self, oid: &Oid) -> Result<BatchResult, StoreError> {
        let mut batch_slot = self.batch.borrow_mut();
        if batch_slot.is_none() {
            *batch_slot = Some(self.spawn_batch()?);
        }
        let batch = batch_slot.as_mut().expect("batch reader was initialized");
        if let Err(error) = writeln!(batch.stdin, "{oid}").and_then(|_| batch.stdin.flush()) {
            return Ok(BatchResult::Dead(error.to_string()));
        }
        let mut header = String::new();
        match batch.stdout.read_line(&mut header) {
            Ok(0) => return Ok(BatchResult::Dead("unexpected end of output".to_string())),
            Ok(_) => {}
            Err(error) => return Ok(BatchResult::Dead(error.to_string())),
        }
        let header = header.trim_end_matches(&['\r', '\n'][..]);
        if header == format!("{oid} missing") {
            return Ok(BatchResult::Missing);
        }
        let fields: Vec<&str> = header.split_whitespace().collect();
        if fields.len() != 3 || fields[0] != oid.as_str() {
            return Err(StoreError::Other(format!(
                "invalid git cat-file response {header:?}"
            )));
        }
        let size = fields[2]
            .parse::<usize>()
            .map_err(|_| StoreError::Other(format!("invalid git cat-file response {header:?}")))?;
        let mut bytes = vec![0; size];
        if let Err(error) = batch.stdout.read_exact(&mut bytes) {
            return Ok(BatchResult::Dead(error.to_string()));
        }
        let mut newline = [0];
        if let Err(error) = batch.stdout.read_exact(&mut newline) {
            return Ok(BatchResult::Dead(error.to_string()));
        }
        if newline[0] != b'\n' {
            return Err(StoreError::Other(
                "invalid git cat-file object terminator".to_string(),
            ));
        }
        Ok(BatchResult::Object {
            kind: fields[1].to_string(),
            bytes,
        })
    }

    fn read_object(&self, oid: &Oid, expected: &'static str) -> Result<Vec<u8>, StoreError> {
        if self.batch_dirty.replace(false) {
            self.reset_batch();
        }
        let mut restarted = false;
        let mut fetched = false;
        loop {
            match self.read_batch_once(oid)? {
                BatchResult::Object { kind, bytes } => {
                    if kind != expected {
                        return Err(StoreError::WrongType {
                            oid: oid.clone(),
                            expected,
                        });
                    }
                    return Ok(bytes);
                }
                BatchResult::Missing if self.remote.is_some() && !fetched => {
                    self.fetch_object(oid).map_err(StoreError::Other)?;
                    fetched = true;
                    self.reset_batch();
                }
                BatchResult::Missing => return Err(StoreError::Missing(oid.clone())),
                BatchResult::Dead(_) if !restarted => {
                    self.reset_batch();
                    if self.remote.is_some()
                        && !fetched
                        && !self.has_local(oid).map_err(StoreError::Other)?
                    {
                        self.fetch_object(oid).map_err(StoreError::Other)?;
                        fetched = true;
                    } else {
                        restarted = true;
                    }
                }
                BatchResult::Dead(error) => {
                    return Err(StoreError::Other(format!(
                        "git cat-file batch reader died: {error}"
                    )))
                }
            }
        }
    }

    fn write_object(&self, kind: ObjectKind, bytes: &[u8]) -> Result<Oid, StoreError> {
        let oid = object_id(kind, bytes);
        let object_dir = self.git_dir.join("objects").join(&oid.as_str()[..2]);
        let object_path = object_dir.join(&oid.as_str()[2..]);
        if object_path.try_exists().map_err(|error| {
            StoreError::Other(format!("checking {}: {error}", object_path.display()))
        })? {
            self.batch_dirty.set(true);
            return Ok(oid);
        }

        fs::create_dir_all(&object_dir).map_err(|error| {
            StoreError::Other(format!("creating {}: {error}", object_dir.display()))
        })?;
        let temporary_path = loop {
            let sequence = NEXT_OBJECT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path =
                object_dir.join(format!(".caos_tmp_obj_{}_{}", std::process::id(), sequence));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => break (path, file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(StoreError::Other(format!(
                        "creating temporary Git object {}: {error}",
                        path.display()
                    )))
                }
            }
        };
        let (temporary_path, file) = temporary_path;
        let write_result = (|| -> Result<(), std::io::Error> {
            let mut encoder = ZlibEncoder::new(file, Compression::default());
            write!(encoder, "{} {}\0", kind.as_str(), bytes.len())?;
            encoder.write_all(bytes)?;
            drop(encoder.finish()?);
            fs::set_permissions(&temporary_path, fs::Permissions::from_mode(0o444))?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary_path);
            return Err(StoreError::Other(format!(
                "writing Git object {}: {error}",
                object_path.display()
            )));
        }

        if object_path.try_exists().map_err(|error| {
            StoreError::Other(format!("checking {}: {error}", object_path.display()))
        })? {
            fs::remove_file(&temporary_path).map_err(|error| {
                StoreError::Other(format!("removing {}: {error}", temporary_path.display()))
            })?;
        } else {
            fs::rename(&temporary_path, &object_path).map_err(|error| {
                let _ = fs::remove_file(&temporary_path);
                StoreError::Other(format!(
                    "installing Git object {}: {error}",
                    object_path.display()
                ))
            })?;
        }
        self.batch_dirty.set(true);
        Ok(oid)
    }
}

impl Drop for GitStore {
    fn drop(&mut self) {
        self.reset_batch();
    }
}

impl ObjectStore for GitStore {
    fn read_blob(&self, oid: &Oid) -> Result<Vec<u8>, StoreError> {
        self.read_object(oid, "blob")
    }

    fn read_tree(&self, oid: &Oid) -> Result<Vec<TreeEntry>, StoreError> {
        let bytes = self.read_object(oid, "tree")?;
        parse_tree_bytes(oid, &bytes)
    }

    fn read_commit(&self, oid: &Oid) -> Result<CommitInfo, StoreError> {
        let bytes = self.read_object(oid, "commit")?;
        parse_commit_bytes(oid, &bytes)
    }

    fn write_blob(&mut self, bytes: &[u8]) -> Result<Oid, StoreError> {
        self.write_object(ObjectKind::Blob, bytes)
    }

    fn write_tree(&mut self, entries: &[TreeEntry]) -> Result<Oid, StoreError> {
        if !is_canonical_tree_order(entries) {
            return Err(StoreError::Other(
                "tree entries are not in canonical order".to_string(),
            ));
        }
        self.write_object(ObjectKind::Tree, &encode_tree_bytes(entries))
    }

    fn write_commit(&mut self, commit: &CommitInfo) -> Result<Oid, StoreError> {
        self.write_object(ObjectKind::Commit, &encode_commit_bytes(commit))
    }
}

impl CodeOps for GitStore {
    fn is_ancestor(&self, ancestor: &Oid, descendant: &Oid) -> Result<bool, String> {
        if ancestor == descendant {
            return Ok(true);
        }
        let output = self.output(&[
            "merge-base",
            "--is-ancestor",
            ancestor.as_str(),
            descendant.as_str(),
        ])?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(self.output_error("git merge-base", &output)),
        }
    }

    fn tree_of(&self, commit: &Oid) -> Result<Oid, String> {
        self.read_commit(commit)
            .map(|commit| commit.tree)
            .map_err(String::from)
    }

    fn merge(&mut self, base: &Oid, ours: &Oid, theirs: &Oid) -> Result<MergeOutcome, String> {
        let base_commit = self.read_commit(base).map_err(String::from)?;
        let ours_tree = self.tree_of(ours)?;
        let theirs_tree = self.tree_of(theirs)?;
        let synthetic_head = |tree: Oid, message: &[u8]| CommitInfo {
            tree,
            parents: vec![base.clone()],
            author: base_commit.author.clone(),
            committer: base_commit.committer.clone(),
            extra_headers: Vec::new(),
            message: message.to_vec(),
        };
        let ours_head = self
            .write_commit(&synthetic_head(ours_tree, b"caos merge ours\n"))
            .map_err(String::from)?;
        let theirs_head = self
            .write_commit(&synthetic_head(theirs_tree, b"caos merge theirs\n"))
            .map_err(String::from)?;
        let output = self.output(&[
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            ours_head.as_str(),
            theirs_head.as_str(),
        ])?;
        if !matches!(output.status.code(), Some(0 | 1)) {
            return Err(self.output_error("git merge-tree", &output));
        }
        let stdout = String::from_utf8(output.stdout)
            .map_err(|_| "git merge-tree returned non-UTF-8 output".to_string())?;
        let mut lines = stdout.lines();
        let tree = lines
            .next()
            .ok_or_else(|| "git merge-tree returned no tree".to_string())?;
        let tree = Oid::parse(tree, "merged tree")?;
        if output.status.success() {
            Ok(MergeOutcome::Merged { tree })
        } else {
            let mut paths: Vec<String> = lines
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect();
            paths.sort();
            paths.dedup();
            Ok(MergeOutcome::Conflict { paths })
        }
    }

    fn commit(
        &mut self,
        tree: &Oid,
        parents: &[Oid],
        message: &str,
        signature: &Signature,
    ) -> Result<Oid, String> {
        self.write_commit(&CommitInfo {
            tree: tree.clone(),
            parents: parents.to_vec(),
            author: signature.clone(),
            committer: signature.clone(),
            extra_headers: Vec::new(),
            message: message.as_bytes().to_vec(),
        })
        .map_err(String::from)
    }

    fn implementation(&self) -> String {
        format!(
            "git-merge-tree/{}",
            self.git_version().unwrap_or_else(|_| "unknown".to_string())
        )
    }
}

#[cfg(all(test, feature = "git-cli"))]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::v3::fixtures::golden;
    use crate::v3::oid::{ensure_genesis, g3};
    use crate::v3::reconcile::{reconcile, RECONCILE_MESSAGE};
    use crate::v3::records::WorkspaceResolution;
    use crate::v3::tree::{canonical_tree_order, Mode, Snapshot, TreeBuilder};
    use crate::v3::validate_spine;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> TestDirectory {
            let path = std::env::temp_dir().join(format!(
                "conversation-protocol-git-{label}-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create test directory");
            TestDirectory(path)
        }

        fn child(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove test directory");
        }
    }

    fn init(path: &Path, bare: bool) {
        let mut command = Command::new("git");
        command.args(["init", "--quiet", "--object-format=sha1"]);
        if bare {
            command.arg("--bare");
        }
        let output = command.arg(path).output().expect("run git init");
        assert!(
            output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn add_remote(repository: &Path, remote: &Path) {
        let remote = format!("file://{}", remote.display());
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["remote", "add", "caos"])
            .arg(&remote)
            .output()
            .expect("add git remote");
        assert!(
            output.status.success(),
            "git remote add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_output(repository: &Path, arguments: &[&str]) -> Output {
        Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(arguments)
            .output()
            .expect("run git")
    }

    fn git_output_with_input(repository: &Path, arguments: &[&str], input: &[u8]) -> Output {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git");
        child
            .stdin
            .as_mut()
            .expect("git stdin")
            .write_all(input)
            .expect("write git stdin");
        drop(child.stdin.take());
        child.wait_with_output().expect("wait for git")
    }

    fn require_git(action: &str, output: Output) -> Output {
        assert!(
            output.status.success(),
            "{action} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn git_stdout(repository: &Path, arguments: &[&str]) -> Vec<u8> {
        require_git("git command", git_output(repository, arguments)).stdout
    }

    fn hash_object(repository: &Path, kind: &str, bytes: &[u8]) -> Oid {
        let output = require_git(
            "git hash-object",
            git_output_with_input(repository, &["hash-object", "-t", kind, "--stdin"], bytes),
        );
        Oid::parse(
            String::from_utf8(output.stdout)
                .expect("hash-object output is UTF-8")
                .trim(),
            "git hash-object oid",
        )
        .expect("parse hash-object oid")
    }

    fn loose_object_path(store: &GitStore, oid: &Oid) -> PathBuf {
        store
            .git_dir
            .join("objects")
            .join(&oid.as_str()[..2])
            .join(&oid.as_str()[2..])
    }

    fn object_inventory(repository: &Path) -> HashSet<String> {
        let output = git_stdout(
            repository,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname) %(objecttype)",
            ],
        );
        String::from_utf8(output)
            .expect("object inventory is UTF-8")
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn configure_object_fetch(repository: &Path) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["config", "uploadpack.allowAnySHA1InWant", "true"])
            .output()
            .expect("configure git remote");
        assert!(output.status.success());
    }

    fn signature() -> Signature {
        Signature {
            name: "Git Store".to_string(),
            email: "git-store@example.com".to_string(),
            time: 1_700_000_000,
            offset: "+0000".to_string(),
        }
    }

    fn oid(character: char) -> Oid {
        Oid::parse(&character.to_string().repeat(40), "test oid").unwrap()
    }

    fn write_commit(store: &mut GitStore, tree: &Oid, parents: &[Oid], message: &str) -> Oid {
        store
            .write_commit(&CommitInfo {
                tree: tree.clone(),
                parents: parents.to_vec(),
                author: signature(),
                committer: signature(),
                extra_headers: Vec::new(),
                message: message.as_bytes().to_vec(),
            })
            .expect("write commit")
    }

    fn update_tree(store: &mut GitStore, base: Option<&Oid>, path: &str, value: &[u8]) -> Oid {
        let mut builder = TreeBuilder::from(base.cloned());
        builder.put(path, Mode::Blob, value.to_vec());
        builder.build(store).expect("build tree")
    }

    fn remote_and_client(label: &str) -> (TestDirectory, PathBuf, PathBuf) {
        let directory = TestDirectory::new(label);
        let remote = directory.child("remote.git");
        let client = directory.child("client");
        init(&remote, true);
        init(&client, false);
        add_remote(&client, &remote);
        (directory, remote, client)
    }

    #[test]
    fn write_and_read_round_trip() {
        let directory = TestDirectory::new("round-trip");
        let repository = directory.child("repository");
        init(&repository, false);
        let mut store = GitStore::open(&repository, None).expect("open git store");

        let first = store.write_blob(b"first\n").expect("write first blob");
        let second = store.write_blob(b"second\n").expect("write second blob");
        let mut entries = vec![
            TreeEntry {
                name: "second".to_string(),
                mode: Mode::Executable,
                oid: second.clone(),
            },
            TreeEntry {
                name: "first".to_string(),
                mode: Mode::Blob,
                oid: first.clone(),
            },
        ];
        canonical_tree_order(&mut entries);
        let tree = store.write_tree(&entries).expect("write tree");
        let commit_info = CommitInfo {
            tree: tree.clone(),
            parents: Vec::new(),
            author: signature(),
            committer: signature(),
            extra_headers: Vec::new(),
            message: b"round trip\n".to_vec(),
        };
        let commit = store.write_commit(&commit_info).expect("write commit");

        assert_eq!(store.read_blob(&first).unwrap(), b"first\n");
        assert_eq!(store.read_blob(&second).unwrap(), b"second\n");
        assert_eq!(store.read_tree(&tree).unwrap(), entries);
        assert_eq!(store.read_commit(&commit).unwrap(), commit_info);
        assert_eq!(ensure_genesis(&mut store), Ok(g3()));

        let mut noncanonical = store.read_tree(&tree).unwrap();
        noncanonical.reverse();
        assert_eq!(
            store.write_tree(&noncanonical),
            Err(StoreError::Other(
                "tree entries are not in canonical order".to_string()
            ))
        );
    }

    #[test]
    fn loose_objects_match_git_and_pass_strict_fsck() {
        let directory = TestDirectory::new("loose-objects");
        let repository = directory.child("repository");
        init(&repository, false);
        let mut store = GitStore::open(&repository, None).expect("open git store");

        let blob_bytes = b"loose object contents\n";
        let blob = store.write_blob(blob_bytes).expect("write blob");
        assert_eq!(blob, hash_object(&repository, "blob", blob_bytes));

        let entries = vec![TreeEntry {
            name: "message.txt".to_string(),
            mode: Mode::Blob,
            oid: blob.clone(),
        }];
        let tree_bytes = encode_tree_bytes(&entries);
        let tree = store.write_tree(&entries).expect("write tree");
        assert_eq!(tree, hash_object(&repository, "tree", &tree_bytes));

        let commit_info = CommitInfo {
            tree: tree.clone(),
            parents: Vec::new(),
            author: signature(),
            committer: signature(),
            extra_headers: Vec::new(),
            message: b"loose commit\n".to_vec(),
        };
        let commit_bytes = encode_commit_bytes(&commit_info);
        let commit = store.write_commit(&commit_info).expect("write commit");
        assert_eq!(commit, hash_object(&repository, "commit", &commit_bytes));

        for (oid, kind) in [(&blob, "blob"), (&tree, "tree"), (&commit, "commit")] {
            assert_eq!(
                git_stdout(&repository, &["cat-file", "-t", oid.as_str()]),
                format!("{kind}\n").as_bytes()
            );
            assert_eq!(
                git_stdout(&repository, &["rev-parse", oid.as_str()]),
                format!("{oid}\n").as_bytes()
            );
            let mode = fs::metadata(loose_object_path(&store, oid))
                .expect("loose object metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o444);
        }
        assert_eq!(
            git_stdout(&repository, &["cat-file", "-p", blob.as_str()]),
            blob_bytes
        );
        let pretty_tree = git_stdout(&repository, &["cat-file", "-p", tree.as_str()]);
        assert_eq!(
            pretty_tree,
            format!("100644 blob {blob}\tmessage.txt\n").as_bytes()
        );
        assert_eq!(
            git_stdout(&repository, &["cat-file", "-p", commit.as_str()]),
            commit_bytes
        );
        require_git(
            "git fsck --strict",
            git_output(&repository, &["fsck", "--strict"]),
        );
    }

    #[test]
    fn writing_an_existing_loose_object_is_idempotent() {
        let directory = TestDirectory::new("idempotent-object");
        let repository = directory.child("repository");
        init(&repository, false);
        let mut store = GitStore::open(&repository, None).expect("open git store");

        let first = store.write_blob(b"same object\n").expect("write object");
        let path = loose_object_path(&store, &first);
        let first_inode = fs::metadata(&path).expect("first metadata").ino();
        let second = store.write_blob(b"same object\n").expect("rewrite object");
        let second_inode = fs::metadata(&path).expect("second metadata").ino();

        assert_eq!(first, second);
        assert_eq!(first_inode, second_inode);
    }

    #[test]
    fn write_then_batch_read_restarts_at_most_once() {
        let directory = TestDirectory::new("batch-after-write");
        let repository = directory.child("repository");
        init(&repository, false);
        let mut store = GitStore::open(&repository, None).expect("open git store");

        let first = store.write_blob(b"first\n").expect("write first blob");
        assert_eq!(store.read_blob(&first).unwrap(), b"first\n");
        let commands_after_first_read = store.command_count.get();

        let second = store.write_blob(b"second\n").expect("write second blob");
        assert_eq!(store.command_count.get(), commands_after_first_read);
        // Git builds differ on whether an existing batch process sees a newly
        // installed loose object. Probe that process directly, then verify the
        // public read conservatively restarts it once in either case.
        match store.read_batch_once(&second).unwrap() {
            BatchResult::Object { kind, bytes } => {
                assert_eq!(kind, "blob");
                assert_eq!(bytes, b"second\n");
            }
            BatchResult::Missing => {}
            BatchResult::Dead(error) => panic!("batch reader died: {error}"),
        }
        assert_eq!(store.read_blob(&second).unwrap(), b"second\n");
        assert_eq!(store.command_count.get(), commands_after_first_read + 1);
        assert_eq!(store.read_blob(&first).unwrap(), b"first\n");
        assert_eq!(store.command_count.get(), commands_after_first_read + 1);
    }

    #[test]
    fn writing_blobs_spawns_no_git_processes() {
        let directory = TestDirectory::new("write-without-processes");
        let repository = directory.child("repository");
        init(&repository, false);
        let mut store = GitStore::open(&repository, None).expect("open git store");
        let commands_before_writes = store.command_count.get();

        for index in 0..200 {
            store
                .write_blob(format!("blob {index}\n").as_bytes())
                .expect("write blob");
        }

        assert_eq!(store.command_count.get(), commands_before_writes);
    }

    #[test]
    fn loose_commit_pushes_to_a_strictly_valid_remote() {
        let (_directory, remote, client) = remote_and_client("loose-push");
        let mut store = GitStore::open(&client, Some("caos")).expect("open git store");
        let blob = store.write_blob(b"pushed contents\n").expect("write blob");
        let tree = store
            .write_tree(&[TreeEntry {
                name: "pushed.txt".to_string(),
                mode: Mode::Blob,
                oid: blob,
            }])
            .expect("write tree");
        let commit = write_commit(&mut store, &tree, &[], "pushed loose commit\n");
        let refname = "refs/caos/test/loose-push";

        store
            .push(&[RefUpdate {
                refname: refname.to_string(),
                expected: None,
                new: Some(commit.clone()),
            }])
            .expect("push loose commit");

        assert_eq!(
            git_stdout(&remote, &["rev-parse", refname]),
            format!("{commit}\n").as_bytes()
        );
        require_git(
            "remote git fsck",
            git_output(&remote, &["fsck", "--strict"]),
        );
    }

    #[test]
    fn pushed_remote_tip_ignores_trees_and_prefers_conversation_heads() {
        let (_directory, _remote, client) = remote_and_client("push-remote-tip");
        let mut store = GitStore::open(&client, Some("caos")).expect("open git store");
        let tree = store.write_tree(&[]).expect("write empty tree");
        let commit = write_commit(&mut store, &tree, &[], "conversation head\n");

        store
            .push(&[RefUpdate {
                refname: format!("refs/caos/req/{tree}"),
                expected: None,
                new: Some(tree.clone()),
            }])
            .expect("push request tree");
        assert_eq!(*store.remote_tip.borrow(), None);

        store
            .push(&[
                RefUpdate {
                    refname: "refs/caos/req/another-tree".to_string(),
                    expected: None,
                    new: Some(tree),
                },
                RefUpdate {
                    refname: "refs/caos/v3/conversations/test/head".to_string(),
                    expected: None,
                    new: Some(commit.clone()),
                },
            ])
            .expect("push conversation head after request tree");
        assert_eq!(*store.remote_tip.borrow(), Some(commit));
    }

    #[test]
    fn push_with_lease_and_read_ref() {
        let (_directory, _remote, client) = remote_and_client("push");
        let mut store = GitStore::open(&client, Some("caos")).expect("open git store");
        let tree = store.write_tree(&[]).expect("write empty tree");
        let first = write_commit(&mut store, &tree, &[], "first\n");
        let second = write_commit(&mut store, &tree, std::slice::from_ref(&first), "second\n");
        let main = "refs/caos/test/main";

        store
            .push(&[RefUpdate {
                refname: main.to_string(),
                expected: None,
                new: Some(first.clone()),
            }])
            .expect("push new ref");
        assert_eq!(store.read_ref(main).unwrap(), Some(first.clone()));
        assert!(store
            .push(&[RefUpdate {
                refname: main.to_string(),
                expected: Some(oid('f')),
                new: Some(second.clone()),
            }])
            .is_err());
        assert_eq!(store.read_ref(main).unwrap(), Some(first.clone()));

        store
            .push(&[RefUpdate {
                refname: main.to_string(),
                expected: Some(first.clone()),
                new: None,
            }])
            .expect("delete ref");
        assert_eq!(store.read_ref(main).unwrap(), None);

        let left = "refs/caos/test/left";
        let right = "refs/caos/test/right";
        store
            .push(&[
                RefUpdate {
                    refname: left.to_string(),
                    expected: None,
                    new: Some(first.clone()),
                },
                RefUpdate {
                    refname: right.to_string(),
                    expected: None,
                    new: Some(second.clone()),
                },
            ])
            .expect("atomic push");
        assert_eq!(store.read_ref(left).unwrap(), Some(first));
        assert_eq!(store.read_ref(right).unwrap(), Some(second));
    }

    #[test]
    fn fetch_after_push_transfers_no_new_objects() {
        let (_directory, _remote, client) = remote_and_client("push-fetch");
        let mut store = GitStore::open(&client, Some("caos")).expect("open git store");
        let tree = update_tree(&mut store, None, "file.txt", b"contents\n");
        let commit = write_commit(&mut store, &tree, &[], "commit\n");
        let refname = "refs/caos/test/push-fetch";

        store
            .push(&[RefUpdate {
                refname: refname.to_string(),
                expected: None,
                new: Some(commit.clone()),
            }])
            .expect("push commit");
        let before = object_inventory(&client);
        let commands_before = store.command_count.get();

        assert_eq!(store.fetch_ref(refname).unwrap(), Some(commit));
        assert_eq!(store.command_count.get(), commands_before + 1);
        assert_eq!(object_inventory(&client), before);
    }

    #[test]
    fn negotiation_tip_fetch_matches_fresh_noop_fetch() {
        let (directory, _remote, writer) = remote_and_client("negotiated-fetch");
        let mut writer_store =
            GitStore::open(&writer, Some("caos")).expect("open writer git store");
        let base_tree = update_tree(&mut writer_store, None, "base.txt", b"base\n");
        let base = write_commit(&mut writer_store, &base_tree, &[], "base\n");
        let refname = "refs/caos/test/negotiated-fetch";
        writer_store
            .push(&[RefUpdate {
                refname: refname.to_string(),
                expected: None,
                new: Some(base.clone()),
            }])
            .expect("push base");

        let negotiated = directory.child("negotiated");
        init(&negotiated, false);
        add_remote(&negotiated, &directory.child("remote.git"));
        let negotiated_store =
            GitStore::open(&negotiated, Some("caos")).expect("open negotiated store");
        assert_eq!(
            negotiated_store.fetch_ref(refname).unwrap(),
            Some(base.clone())
        );

        let tip_tree = update_tree(&mut writer_store, Some(&base_tree), "tip.txt", b"tip\n");
        let tip = write_commit(
            &mut writer_store,
            &tip_tree,
            std::slice::from_ref(&base),
            "tip\n",
        );
        writer_store
            .push(&[RefUpdate {
                refname: refname.to_string(),
                expected: Some(base),
                new: Some(tip.clone()),
            }])
            .expect("advance remote");
        assert_eq!(
            negotiated_store.fetch_ref(refname).unwrap(),
            Some(tip.clone())
        );

        let noop = directory.child("noop");
        init(&noop, false);
        add_remote(&noop, &directory.child("remote.git"));
        let noop_store = GitStore::open(&noop, Some("caos")).expect("open noop store");
        assert_eq!(noop_store.fetch_ref(refname).unwrap(), Some(tip));
        assert_eq!(object_inventory(&negotiated), object_inventory(&noop));
        require_git(
            "negotiated git fsck",
            git_output(&negotiated, &["fsck", "--strict"]),
        );
        require_git("noop git fsck", git_output(&noop, &["fsck", "--strict"]));
    }

    #[test]
    fn handwritten_scratch_repo_is_accepted_by_git() {
        let directory = TestDirectory::new("scratch-layout");
        let remote = directory.child("remote.git");
        init(&remote, true);
        let scratch_name = format!(
            "conversation-protocol-scratch-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        );
        let scratch_path = PathBuf::from(format!("/tmp/{scratch_name}"));
        let remote_url = format!("file://{}", remote.display());
        let scratch =
            GitStore::scratch(&scratch_name, &remote_url).expect("create handwritten scratch");
        assert_eq!(scratch.command_count.get(), 0);
        assert_eq!(
            git_stdout(&scratch_path, &["rev-parse", "--git-dir"]),
            b".\n"
        );

        let source = directory.child("source");
        init(&source, false);
        let mut source_store = GitStore::open(&source, None).expect("open source");
        let tree = update_tree(&mut source_store, None, "file.txt", b"contents\n");
        let commit = write_commit(&mut source_store, &tree, &[], "scratch push\n");
        let destination = format!("file://{}", scratch_path.display());
        require_git(
            "push into handwritten scratch",
            git_output(
                &source,
                &[
                    "push",
                    &destination,
                    &format!("{commit}:refs/heads/accepted"),
                ],
            ),
        );
        require_git(
            "handwritten scratch git fsck",
            git_output(&scratch_path, &["fsck", "--strict"]),
        );

        drop(scratch);
        fs::remove_dir_all(&scratch_path).expect("remove handwritten scratch");
    }

    #[test]
    fn fetch_ref_includes_blobs_and_missing_object_is_lazy_fetched() {
        let (directory, remote, first_client) = remote_and_client("lazy-fetch");
        configure_object_fetch(&remote);
        let mut first_store =
            GitStore::open(&first_client, Some("caos")).expect("open first git store");
        let fetched_blob = first_store
            .write_blob(b"fetched blob\n")
            .expect("write fetched blob");
        let fetched_tree = first_store
            .write_tree(&[TreeEntry {
                name: "fetched.txt".to_string(),
                mode: Mode::Blob,
                oid: fetched_blob.clone(),
            }])
            .expect("write fetched tree");
        let fetched_commit = write_commit(&mut first_store, &fetched_tree, &[], "fetched\n");
        let lazy_blob = first_store
            .write_blob(b"lazy blob\n")
            .expect("write lazy blob");
        let lazy_tree = first_store
            .write_tree(&[TreeEntry {
                name: "lazy.txt".to_string(),
                mode: Mode::Blob,
                oid: lazy_blob.clone(),
            }])
            .expect("write lazy tree");
        let lazy_commit = write_commit(&mut first_store, &lazy_tree, &[], "lazy\n");
        let fetched_ref = "refs/caos/test/fetched";
        let lazy_ref = "refs/caos/test/lazy";
        first_store
            .push(&[
                RefUpdate {
                    refname: fetched_ref.to_string(),
                    expected: None,
                    new: Some(fetched_commit.clone()),
                },
                RefUpdate {
                    refname: lazy_ref.to_string(),
                    expected: None,
                    new: Some(lazy_commit),
                },
            ])
            .expect("push refs");

        let second_client = directory.child("second-client");
        init(&second_client, false);
        add_remote(&second_client, &remote);
        let second_store =
            GitStore::open(&second_client, Some("caos")).expect("open second git store");
        assert_eq!(
            second_store.fetch_ref(fetched_ref).unwrap(),
            Some(fetched_commit.clone())
        );
        assert!(second_store.has_local(&fetched_commit).unwrap());
        assert!(second_store.has_local(&fetched_tree).unwrap());
        assert!(second_store.has_local(&fetched_blob).unwrap());
        assert!(!second_store.has_local(&lazy_blob).unwrap());
        assert_eq!(second_store.read_blob(&lazy_blob).unwrap(), b"lazy blob\n");
        assert!(second_store.has_local(&lazy_blob).unwrap());
    }

    #[test]
    fn fetch_absent_ref_returns_none() {
        let (_directory, _remote, client) = remote_and_client("absent-ref");
        let store = GitStore::open(&client, Some("caos")).expect("open git store");

        assert_eq!(store.fetch_ref("refs/caos/test/absent"), Ok(None));
    }

    #[test]
    fn golden_spine_uses_real_git_objects() {
        let directory = TestDirectory::new("golden");
        let repository = directory.child("repository");
        init(&repository, false);
        let mut store = GitStore::open(&repository, None).expect("open git store");
        let head = golden(&mut store);
        let validated = validate_spine(&store, &head, &mut HashSet::new()).expect("validate spine");
        assert!(!validated.is_empty());
    }

    #[test]
    fn code_ops_over_real_history() {
        let directory = TestDirectory::new("code-ops");
        let repository = directory.child("repository");
        init(&repository, false);
        let mut store = GitStore::open(&repository, None).expect("open git store");
        let base_tree = update_tree(&mut store, None, "shared.txt", b"base\n");
        // GitHub's signed commits are ordinary workspace inputs. Keep their
        // headers and object identity through reads, writes, and merges.
        let base_bytes = format!(
            "tree {base_tree}\nauthor Git Store <git-store@example.com> 1700000000 +0000\n\
             committer Git Store <git-store@example.com> 1700000000 +0000\n\
             encoding UTF-8\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n signed data\n \
             -----END PGP SIGNATURE-----\n\nbase\n"
        )
        .into_bytes();
        let base = store.write_object(ObjectKind::Commit, &base_bytes).unwrap();
        let base_info = store.read_commit(&base).expect("read signed code commit");
        assert_eq!(encode_commit_bytes(&base_info), base_bytes);
        assert_eq!(store.write_commit(&base_info).unwrap(), base);
        assert_eq!(base, hash_object(&repository, "commit", &base_bytes));
        assert_eq!(store.tree_of(&base).unwrap(), base_tree);
        let ours_tree = update_tree(&mut store, Some(&base_tree), "ours.txt", b"ours\n");
        let ours = write_commit(
            &mut store,
            &ours_tree,
            std::slice::from_ref(&base),
            "ours\n",
        );
        let theirs_tree = update_tree(&mut store, Some(&base_tree), "theirs.txt", b"theirs\n");
        let theirs = write_commit(
            &mut store,
            &theirs_tree,
            std::slice::from_ref(&base),
            "theirs\n",
        );

        assert!(store.is_ancestor(&base, &base).unwrap());
        assert!(store.is_ancestor(&base, &ours).unwrap());
        assert!(!store.is_ancestor(&ours, &base).unwrap());
        assert!(!store.is_ancestor(&ours, &theirs).unwrap());
        let merge_tree = match store.merge(&base, &ours, &theirs).unwrap() {
            MergeOutcome::Merged { tree } => tree,
            MergeOutcome::Conflict { paths } => panic!("unexpected conflict: {paths:?}"),
        };
        let snapshot = Snapshot::new(&store, merge_tree.clone());
        assert_eq!(snapshot.read("ours.txt").unwrap(), Some(b"ours\n".to_vec()));
        assert_eq!(
            snapshot.read("theirs.txt").unwrap(),
            Some(b"theirs\n".to_vec())
        );

        let conflict_ours_tree = update_tree(&mut store, Some(&base_tree), "shared.txt", b"ours\n");
        let conflict_ours = write_commit(
            &mut store,
            &conflict_ours_tree,
            std::slice::from_ref(&base),
            "conflict ours\n",
        );
        let conflict_theirs_tree =
            update_tree(&mut store, Some(&base_tree), "shared.txt", b"theirs\n");
        let conflict_theirs = write_commit(
            &mut store,
            &conflict_theirs_tree,
            std::slice::from_ref(&base),
            "conflict theirs\n",
        );
        assert_eq!(
            store
                .merge(&base, &conflict_ours, &conflict_theirs)
                .unwrap(),
            MergeOutcome::Conflict {
                paths: vec!["shared.txt".to_string()]
            }
        );

        assert_eq!(
            reconcile(&mut store, &base, &theirs, Some(&base), &signature()).unwrap(),
            WorkspaceResolution::Direct {
                current: base.clone(),
                output: theirs.clone(),
            }
        );

        let merged = reconcile(&mut store, &base, &theirs, Some(&ours), &signature()).unwrap();
        let merged_output = match merged {
            WorkspaceResolution::Merged { merge, output, .. } => {
                assert_eq!(merge.output, Some(output.clone()));
                assert!(merge.implementation.starts_with("git-merge-tree/"));
                output
            }
            resolution => panic!("expected merged resolution, got {resolution:?}"),
        };
        let merged_commit = store.read_commit(&merged_output).unwrap();
        assert_eq!(merged_commit.parents, vec![ours.clone(), theirs.clone()]);
        assert_eq!(merged_commit.tree, merge_tree);
        assert_eq!(merged_commit.message, RECONCILE_MESSAGE.as_bytes());

        assert!(matches!(
            reconcile(
                &mut store,
                &base,
                &conflict_theirs,
                Some(&conflict_ours),
                &signature()
            )
            .unwrap(),
            WorkspaceResolution::Conflict { merge: Some(_), .. }
        ));

        let root_tree = store.write_tree(&[]).unwrap();
        let root = write_commit(&mut store, &root_tree, &[], "root\n");
        let rollback_base_tree = update_tree(&mut store, Some(&root_tree), "base.txt", b"base\n");
        let rollback_base = write_commit(
            &mut store,
            &rollback_base_tree,
            std::slice::from_ref(&root),
            "rollback base\n",
        );
        let rollback_proposal_tree = update_tree(
            &mut store,
            Some(&rollback_base_tree),
            "proposal.txt",
            b"proposal\n",
        );
        let rollback_proposal = write_commit(
            &mut store,
            &rollback_proposal_tree,
            std::slice::from_ref(&rollback_base),
            "rollback proposal\n",
        );
        assert!(matches!(
            reconcile(
                &mut store,
                &rollback_base,
                &rollback_proposal,
                Some(&root),
                &signature()
            )
            .unwrap(),
            WorkspaceResolution::Merged { .. }
        ));

        let equal_tree = update_tree(&mut store, Some(&base_tree), "equal.txt", b"equal\n");
        let equal_current = write_commit(
            &mut store,
            &equal_tree,
            std::slice::from_ref(&base),
            "equal current\n",
        );
        let equal_proposal = write_commit(
            &mut store,
            &equal_tree,
            std::slice::from_ref(&base),
            "equal proposal\n",
        );
        let output = match reconcile(
            &mut store,
            &base,
            &equal_proposal,
            Some(&equal_current),
            &signature(),
        )
        .unwrap()
        {
            WorkspaceResolution::Merged { output, .. } => output,
            other => panic!("equal trees must retain both histories: {other:?}"),
        };
        assert_eq!(store.tree_of(&output).unwrap(), equal_tree);
        assert!(store.is_ancestor(&equal_current, &output).unwrap());
        assert!(store.is_ancestor(&equal_proposal, &output).unwrap());

        // A merge can add ancestry without changing the workspace tree.
        let upstream = write_commit(
            &mut store,
            &base_tree,
            std::slice::from_ref(&base),
            "upstream metadata\n",
        );
        let proposal = write_commit(
            &mut store,
            &ours_tree,
            &[ours.clone(), upstream.clone()],
            "merge upstream\n",
        );
        let output =
            match reconcile(&mut store, &ours, &proposal, Some(&ours), &signature()).unwrap() {
                WorkspaceResolution::Direct { output, .. } => output,
                other => panic!("merge ancestry must advance the workspace: {other:?}"),
            };
        assert_eq!(output, proposal);
        assert_eq!(store.tree_of(&output).unwrap(), ours_tree);
        assert!(store.is_ancestor(&upstream, &output).unwrap());
    }
}
