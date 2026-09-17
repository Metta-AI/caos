//! Fetch an exact commit into the server ODB. Completion markers certify full
//! object closure; callers own ref resolution and invocation state.
use crate::{Config, HttpError};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::Path;

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
    // imports are negotiation tips: an existing commit can lack trees or blobs.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("lock"))?;
    lock.lock()?;
    let complete = directory.join(format!("{}.complete", input.commit));
    if !complete.exists() {
        let tips: Vec<_> = fs::read_dir(&directory)?
            .map(|entry| {
                let name = entry?.file_name();
                Ok(name
                    .to_str()
                    .and_then(|n| n.strip_suffix(".complete"))
                    .filter(|c| git_locator::import::commit(c))
                    .map(str::to_owned))
            })
            .collect::<Result<Vec<_>, std::io::Error>>()?
            .into_iter()
            .flatten()
            .collect();
        let shallow = directory.join(format!("{}.shallow", input.commit));
        let run = |args: &[&str]| -> Result<String, HttpError> {
            let output = git_locator::import::git(&input.source, token)
                .map_err(failure)?
                .env("GIT_SHALLOW_FILE", &shallow)
                .args(["--git-dir", &config.git_dir])
                .args(args)
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
        // Never let a shallow upstream change the shared ODB's boundary file.
        File::create(&shallow)?;
        run(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
        match fs::read(&shallow) {
            Ok(bytes) if !bytes.is_empty() => {
                return Err(failure("remote did not supply complete ancestor history"))
            }
            Ok(_) => fs::remove_file(&shallow)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if run(&["cat-file", "-t", &input.commit])?.trim() != "commit" {
            return Err(failure("remote object is not a commit"));
        }
        let closure = run(&[
            "rev-list",
            "--objects",
            "--no-object-names",
            &input.commit,
            "--",
        ])?;
        for oid in closure.lines() {
            crate::storage::get_object(config, oid)
                .map_err(|_| failure("imported object not visible through object storage"))?;
        }
        // GC is disabled on the server. This records verified closure, not a
        // GC root or an invocation; a hit needs no remote credential check.
        File::create(&complete)?.sync_all()?;
        File::open(&directory)?.sync_all()?;
    }
    Ok(serde_json::to_vec(&serde_json::json!({"commit": input.commit})).unwrap())
}
