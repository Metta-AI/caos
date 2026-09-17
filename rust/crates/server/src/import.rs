//! Fetch an exact commit into the server ODB. Completion markers certify full
//! object closure; callers own ref resolution and invocation state.
use crate::{Config, HttpError};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    source: String,
    commit: String,
}

fn failure(message: impl Into<String>) -> HttpError {
    HttpError::new(502, message)
}

pub(crate) fn endpoint(
    config: &Config,
    request: &mut tiny_http::Request,
) -> Result<Vec<u8>, HttpError> {
    let tokens: Vec<_> = request
        .headers()
        .iter()
        .filter(|h| h.field.equiv(git_locator::import::TOKEN_HEADER))
        .map(|h| h.value.as_str().to_owned())
        .collect();
    if tokens.len() > 1 {
        return Err(HttpError::new(400, "duplicate Git token header"));
    }
    let mut body = Vec::new();
    request.as_reader().take(16385).read_to_end(&mut body)?;
    if body.len() > 16384 {
        return Err(HttpError::new(400, "import request too large"));
    }
    let mut input: Input =
        serde_json::from_slice(&body).map_err(|_| HttpError::new(400, "invalid import request"))?;
    if !git_locator::import::commit(&input.commit) {
        return Err(HttpError::new(400, "import requires a full commit hash"));
    }
    input.commit.make_ascii_lowercase();
    let token = tokens.first().map(String::as_str);
    // Validate credentials and URL even on a cache hit.
    git_locator::import::git(&input.source, token).map_err(|e| HttpError::new(400, e))?;

    let root = Path::new(&config.git_dir).join("caos-imports");
    let directory = root.join(format!("{:x}", Sha256::digest(input.source.as_bytes())));
    fs::create_dir_all(&directory)?;
    File::open(&config.git_dir)?.sync_all()?;
    File::open(&root)?.sync_all()?;
    // Coordinate separate server processes sharing the ODB. Only complete
    // imports from this URL are negotiation tips, avoiding unrelated histories.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("lock"))?;
    lock.lock()?;
    let complete = directory.join(format!("{}.complete", input.commit));
    if !complete.exists() {
        let mut tips = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let name = entry?.file_name();
            if let Some(commit) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".complete"))
            {
                if git_locator::import::commit(commit) {
                    tips.push(commit.to_string());
                }
            }
        }
        // The URL lock also owns this staging directory. A previous process
        // may have died with a partial fetch here; none of it is published.
        let incoming = Incoming::new(&directory, &config.git_dir)?;
        let run = |args: &[&str], stdin_path: Option<&Path>| -> Result<String, HttpError> {
            let mut command = git_locator::import::git(&input.source, token).map_err(failure)?;
            command.args(["--git-dir"]).arg(&incoming.path).args(args);
            if let Some(path) = stdin_path {
                command.stdin(File::open(path)?);
            }
            let output = command
                .output()
                .map_err(|_| failure("could not start Git import"))?;
            if !output.status.success() {
                return Err(failure(
                    "Git import failed; check the commit, repository and credentials",
                ));
            }
            String::from_utf8(output.stdout).map_err(|_| failure("invalid Git response"))
        };
        let mut args = vec![
            "fetch".to_string(),
            "--no-tags".into(),
            "--no-write-fetch-head".into(),
            "--no-auto-maintenance".into(),
            "--no-recurse-submodules".into(),
            "--no-filter".into(),
            // Record every remote boundary in the private repository so it can
            // be rejected below, rather than letting Git silently skip a ref.
            "--update-shallow".into(),
        ];
        if tips.is_empty() {
            args.splice(
                0..0,
                ["-c".into(), "fetch.negotiationAlgorithm=noop".into()],
            );
        } else {
            args.extend(tips.iter().map(|tip| format!("--negotiation-tip={tip}")));
        }
        args.extend(["--".into(), input.source.clone(), input.commit.clone()]);
        run(&args.iter().map(String::as_str).collect::<Vec<_>>(), None)?;
        match fs::read(incoming.path.join("shallow")) {
            Ok(bytes) if !bytes.is_empty() => {
                return Err(failure("remote did not supply complete ancestor history"))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if run(&["cat-file", "-t", &input.commit], None)?.trim() != "commit" {
            return Err(failure("remote object is not a commit"));
        }
        // Every received object must be checked, including surplus roots. A
        // fresh index-pack has no remote shallow exemptions. Its strict check
        // verifies links into the live store by type, then stops there: startup
        // and checked publication guarantee those objects' transitive closure.
        let staged = Config {
            git_dir: incoming.path.to_string_lossy().into_owned(),
            repo: gix::open(&incoming.path)
                .map_err(|_| failure("could not open staged import"))?
                .into_sync(),
            ..config.clone()
        };
        for index in incoming.indexes()? {
            let pack = index.with_extension("pack");
            run(
                &[
                    "index-pack",
                    "--strict",
                    "--threads=1",
                    &pack.to_string_lossy(),
                ],
                None,
            )?;
            // Exercise the same reader used by the live API, only for received
            // objects. Walking their existing ancestors would undo the boundary.
            for line in run(&["show-index"], Some(&index))?.lines() {
                let oid = line
                    .split_whitespace()
                    .nth(1)
                    .filter(|oid| git_locator::import::commit(oid))
                    .ok_or_else(|| failure("invalid imported pack index"))?;
                crate::storage::get_object(&staged, oid)
                    .map_err(|_| failure("imported object not readable in staging"))?;
            }
        }
        crate::storage::get_object(&staged, &input.commit)
            .map_err(|_| failure("imported commit not readable in staging"))?;
        incoming.publish(&config.git_dir)?;
        // Exercise the live handle after pack publication, before certifying H.
        crate::storage::get_object(config, &input.commit)
            .map_err(|_| failure("imported commit not visible through object storage"))?;
        // GC is disabled on the server. This records verified closure, not a
        // GC root or an invocation; a hit needs no remote credential check.
        File::create(&complete)?.sync_all()?;
        File::open(&directory)?.sync_all()?;
    }
    Ok(serde_json::to_vec(&serde_json::json!({"commit": input.commit})).unwrap())
}

/// A private repository with read-only access to previously stored objects.
/// Fetch is forced to keep its one received pack, including tiny transfers:
/// publishing loose objects one at a time could expose a child before a parent.
struct Incoming {
    path: PathBuf,
}

impl Incoming {
    fn new(directory: &Path, git_dir: &str) -> Result<Self, HttpError> {
        let incoming = Self {
            path: directory.join("incoming"),
        };
        if incoming.path.exists() {
            fs::remove_dir_all(&incoming.path)?;
        }
        fs::create_dir_all(incoming.path.join("objects/info"))?;
        fs::create_dir_all(incoming.path.join("objects/pack"))?;
        fs::create_dir_all(incoming.path.join("refs"))?;
        fs::write(incoming.path.join("HEAD"), b"ref: refs/heads/main\n")?;
        fs::write(incoming.path.join("config"), b"[core]\nrepositoryformatversion = 0\nbare = true\nfsync = objects\n[fetch]\nunpackLimit = 1\nfsckObjects = true\n[transfer]\nunpackLimit = 1\n[gc]\nauto = 0\n[maintenance]\nauto = false\n")?;
        let objects = fs::canonicalize(Path::new(git_dir).join("objects"))?;
        fs::write(
            incoming.path.join("objects/info/alternates"),
            format!("{}\n", objects.display()),
        )?;
        Ok(incoming)
    }

    fn indexes(&self) -> Result<Vec<PathBuf>, HttpError> {
        let packs = self.path.join("objects/pack");
        let indexes: Vec<_> = fs::read_dir(&packs)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "idx"))
            .collect();
        // One fetch receives one pack. Refuse an unexpected layout instead of
        // publishing several packs with potentially interdependent histories.
        if indexes.len() > 1 {
            return Err(failure("import produced more than one pack"));
        }
        Ok(indexes)
    }

    fn publish(&self, git_dir: &str) -> Result<(), HttpError> {
        let destination = Path::new(git_dir).join("objects/pack");
        fs::create_dir_all(&destination)?;
        for index in self.indexes()? {
            let pack = index.with_extension("pack");
            File::open(&pack)?.sync_all()?;
            File::open(&index)?.sync_all()?;
            // Readers discover packs through their index. Publish and sync the
            // complete pack before making its index visible. Hard links also
            // preserve a concurrent import of the identical pack.
            for source in [&pack, &index] {
                let target = destination.join(source.file_name().unwrap());
                match fs::hard_link(source, target) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.into()),
                }
                File::open(&destination)?.sync_all()?;
            }
        }
        Ok(())
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            eprintln!(
                "cannot remove import staging directory {}: {error}",
                self.path.display()
            );
        }
    }
}
