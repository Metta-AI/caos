//! Remote imports write objects, never conversations. Records are durable outside
//! Git: pending pins the observation before transfer; complete certifies closure.
use crate::{Config, HttpError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Input {
    source: String,
    revision: Option<String>,
    invocation: String,
    secret_scope: String,
}

#[derive(Deserialize, Serialize)]
struct Record {
    commit: String,
    complete: bool,
    repository: String,
    requested_revision: Option<String>,
    default_branch: Option<String>,
    observed_at: u64,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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
    import(config, &body, tokens.first().map(String::as_str))
}

fn import(config: &Config, body: &[u8], token: Option<&str>) -> Result<Vec<u8>, HttpError> {
    let input: Input =
        serde_json::from_slice(body).map_err(|_| HttpError::new(400, "invalid import request"))?;
    let (host, path) =
        git_locator::import::remote(&input.source).map_err(|e| HttpError::new(400, e))?;
    let revision = git_locator::import::revision(input.revision.as_deref())
        .map_err(|e| HttpError::new(400, e))?;
    if !(input.invocation.len() == 64 && input.invocation.bytes().all(|b| b.is_ascii_hexdigit()))
        || (!input.secret_scope.is_empty() && !git_locator::import::commit(&input.secret_scope))
        || token.is_some_and(|t| {
            t.is_empty() || t.len() > 8192 || !t.bytes().all(|b| b > b' ' && b < 127)
        })
        || (token.is_some() && input.secret_scope.is_empty())
    {
        return Err(HttpError::new(
            400,
            "invalid import identity or credential scope",
        ));
    }
    // Token bytes never key state. Presence separates unauthenticated calls from
    // authenticated ones even if a caller accidentally reuses a secret scope.
    let scope = digest(
        &serde_json::to_vec(&(&input.source, &input.secret_scope, token.is_some())).unwrap(),
    );
    let root = Path::new(&config.git_dir).join("caos-imports");
    fs::create_dir_all(&root)?;
    let directory = root.join(scope);
    fs::create_dir_all(&directory)?;
    File::open(&config.git_dir)?.sync_all()?;
    File::open(&root)?.sync_all()?;
    // A file lock also coordinates separate server processes sharing this ODB.
    // Serializing one repository/scope makes its complete negotiation tips stable.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("lock"))?;
    lock.lock()?;
    let identity = digest(&serde_json::to_vec(&input).unwrap());
    let record_path = directory.join(format!("{identity}.json"));
    let shallow_path = record_path.with_extension("shallow");
    let git = Git {
        shallow_path: &shallow_path,
        config,
        source: &input.source,
        host,
        path,
        token,
    };
    let mut record: Record = if record_path.exists() {
        serde_json::from_slice(&fs::read(&record_path)?)
            .map_err(|_| HttpError::new(500, "invalid import record"))?
    } else {
        let (commit, default_branch) = if git_locator::import::commit(&revision) {
            (revision.clone(), None)
        } else {
            let result = git.run(&[
                "ls-remote",
                "--symref",
                "--",
                &input.source,
                &revision,
                &format!("{revision}^{{}}"),
            ])?;
            let mut hash = None;
            let mut peeled = None;
            let mut default_branch = None;
            for line in result.lines() {
                if let Some((left, right)) = line.split_once('\t') {
                    if right == revision {
                        if let Some(branch) = left.strip_prefix("ref: refs/heads/") {
                            if revision == "HEAD" {
                                default_branch = Some(branch.to_string());
                            }
                        } else if git_locator::import::commit(left) {
                            hash = Some(left.to_ascii_lowercase());
                        }
                    } else if right == format!("{revision}^{{}}")
                        && git_locator::import::commit(left)
                    {
                        peeled = Some(left.to_ascii_lowercase());
                    }
                }
            }
            (
                peeled
                    .or(hash)
                    .ok_or_else(|| failure("remote revision not found"))?,
                default_branch,
            )
        };
        let record = Record {
            commit,
            complete: false,
            repository: input.source.clone(),
            requested_revision: input.revision.clone(),
            default_branch,
            observed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };
        save(&record_path, &record)?;
        record
    };
    if !record.complete {
        let mut tips = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let prior: Record = serde_json::from_slice(&fs::read(path)?)
                    .map_err(|_| HttpError::new(500, "invalid import record"))?;
                if prior.complete {
                    tips.push(prior.commit);
                }
            }
        }
        tips.sort();
        tips.dedup();
        let mut args = vec![
            "fetch".to_string(),
            "--no-tags".into(),
            "--no-write-fetch-head".into(),
            "--no-auto-maintenance".into(),
            "--no-recurse-submodules".into(),
            "--no-filter".into(),
        ];
        if tips.is_empty() {
            // noop sends no local refs; unlike a made-up negotiation tip it also
            // works in an empty repository.
            args.splice(
                0..0,
                ["-c".into(), "fetch.negotiationAlgorithm=noop".into()],
            );
        } else {
            args.extend(tips.iter().map(|tip| format!("--negotiation-tip={tip}")));
        }
        args.extend(["--".into(), input.source.clone(), record.commit.clone()]);
        // A shallow upstream must not write the server's shared shallow file.
        // Start every attempt with a private empty boundary list, then refuse
        // any boundary the remote returned instead of certifying partial history.
        File::create(&shallow_path)?;
        git.run(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
        match fs::read(&shallow_path) {
            Ok(bytes) if !bytes.is_empty() => {
                return Err(failure("remote did not supply complete ancestor history"))
            }
            Ok(_) => {
                fs::remove_file(&shallow_path)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        // rev-list walks all ancestors AND trees/blobs; mere commit existence is
        // not completion. Read every object through the live storage handle too.
        if git.run(&["cat-file", "-t", &record.commit])?.trim() != "commit" {
            return Err(failure("remote revision is not a commit"));
        }
        let closure = git.run(&[
            "rev-list",
            "--objects",
            "--no-object-names",
            &record.commit,
            "--",
        ])?;
        for oid in closure.lines() {
            crate::storage::get_object(config, oid)
                .map_err(|_| failure("imported object not visible through object storage"))?;
        }
        record.complete = true;
        save(&record_path, &record)?;
    }
    serde_json::to_vec(&record).map_err(|_| HttpError::new(500, "encoding import result"))
}

fn save(path: &Path, record: &Record) -> Result<(), HttpError> {
    let temp = path.with_extension("pending");
    let mut file = File::create(&temp)?;
    file.write_all(&serde_json::to_vec(record).unwrap())?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

struct Git<'a> {
    shallow_path: &'a Path,
    config: &'a Config,
    source: &'a str,
    host: &'a str,
    path: &'a str,
    token: Option<&'a str>,
}
impl Git<'_> {
    fn run(&self, args: &[&str]) -> Result<String, HttpError> {
        // The helper receives only this process's credential context. Refuse
        // redirects and all other protocols; never persist credentials or echo
        // remote-controlled stderr (it can contain credentials).
        const HELPER: &str = "!f() { test \"$1\" = get || exit 0; protocol= host= path=; while IFS='=' read -r k v; do case \"$k\" in protocol) protocol=$v;; host) host=$v;; path) path=$v;; esac; done; if test \"$protocol\" = https && test \"$host\" = \"$CAOS_IMPORT_HOST\" && test \"$path\" = \"$CAOS_IMPORT_PATH\"; then printf 'username=x-access-token\\npassword=%s\\n' \"$CAOS_IMPORT_TOKEN\"; fi; }; f";
        let mut cmd = Command::new("timeout");
        cmd.args(["--kill-after=5", "300", "git"]);
        // Whitelist inherited environment: in particular no Git traces, askpass,
        // config injection, URL rewrites or ambient credential helpers.
        cmd.env_clear();
        for name in ["PATH", "SSL_CERT_FILE", "GIT_SSL_CAINFO"] {
            if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "https")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_SHALLOW_FILE", self.shallow_path)
            .args([
                "--git-dir",
                &self.config.git_dir,
                "-c",
                "credential.helper=",
                "-c",
                "credential.useHttpPath=true",
                "-c",
                "http.followRedirects=false",
                "-c",
                "http.extraHeader=",
                "-c",
                "fetch.writeCommitGraph=false",
            ]);
        if let Some(token) = self.token {
            cmd.env("CAOS_IMPORT_TOKEN", token)
                .env("CAOS_IMPORT_HOST", self.host)
                .env("CAOS_IMPORT_PATH", self.path)
                .args(["-c", &format!("credential.{}.helper={HELPER}", self.source)]);
        }
        let output = cmd
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|_| failure("could not start Git import"))?;
        if !output.status.success() {
            return Err(failure("Git import failed (remote revision, access, connection, or timeout); retry or check the repository and credentials"));
        }
        String::from_utf8(output.stdout).map_err(|_| failure("invalid Git response"))
    }
}
