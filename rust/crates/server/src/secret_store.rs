//! The server-held secret store (SPEC.md, "Secrets"): one tree per
//! SecretReaderKey in `secrets.git`, replaced by a signed push.
//!
//! `secrets.git` is never linked to the main object database — no alternates,
//! no fetch endpoint, nothing a worker can name — and keeps no reflog, so a
//! pruned value is gone.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use caos_world::secrets::{self as fmt, Reader};
use ed25519_dalek::{Signature, VerifyingKey};

use crate::{Config, HttpError};

/// One push at a time: the sequence check and the ref update are one step.
static PUSH: Mutex<()> = Mutex::new(());

/// A secret as the server holds it.
#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub(crate) name: String,
    pub(crate) entropy: String,
    pub(crate) value: String,
    pub(crate) readers: Vec<Reader>,
}

/// `secrets.git` beside the main repository.
pub(crate) fn default_dir(git_dir: &str) -> String {
    Path::new(git_dir)
        .with_file_name("secrets.git")
        .to_string_lossy()
        .into_owned()
}

/// Create `secrets.git` if absent and assert the settings it relies on.
pub(crate) fn init(dir: &str) -> Result<(), String> {
    if gix::open(dir).is_err() {
        git(None, &["init", "-q", "--bare", dir])?;
    }
    git(Some(dir), &["config", "core.logAllRefUpdates", "false"])?;
    git(Some(dir), &["config", "gc.auto", "0"])?;
    std::fs::create_dir_all(sequence_dir(dir))
        .map_err(|e| format!("creating {}: {e}", sequence_dir(dir).display()))
}

/// `POST /secrets/push`: verify, store, replace the key's tree, prune.
pub(crate) fn push_endpoint(
    config: &Config,
    request: &mut tiny_http::Request,
) -> Result<Vec<u8>, HttpError> {
    let header = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().to_string())
            .ok_or_else(|| HttpError::new(400, format!("missing {name}")))
    };
    let reader_key = header(fmt::PUSH_KEY_HEADER)?;
    let sequence: u64 = header(fmt::PUSH_SEQUENCE_HEADER)?
        .parse()
        .map_err(|_| HttpError::new(400, "sequence is not a number"))?;
    let signature = header(fmt::PUSH_SIGNATURE_HEADER)?;
    if !fmt::is_reader_key(&reader_key) {
        return Err(HttpError::new(400, "not a SecretReaderKey"));
    }
    let verifying = unhex::<32>(&reader_key)
        .and_then(|bytes| VerifyingKey::from_bytes(&bytes).ok())
        .ok_or_else(|| HttpError::new(400, "not an ed25519 public key"))?;
    let signature = unhex::<64>(&signature)
        .map(|bytes| Signature::from_bytes(&bytes))
        .ok_or_else(|| HttpError::new(400, "signature is not 128 hex characters"))?;
    let mut body = Vec::new();
    std::io::Read::read_to_end(request.as_reader(), &mut body)?;
    let secrets = parse_body(&body).map_err(|e| HttpError::new(400, e))?;

    let dir = &config.secrets_git;
    // Held across the write too: a prune must never see another push's
    // objects before its ref does.
    let _guard = PUSH.lock().unwrap_or_else(|e| e.into_inner());
    let tree = write_tree(dir, &secrets).map_err(|e| HttpError::new(500, e))?;
    let message = fmt::push_message(&reader_key, &tree, sequence);
    let verified = verifying.verify_strict(&message, &signature).is_ok();
    if verified {
        let last = last_sequence(dir, &reader_key).map_err(|e| HttpError::new(500, e))?;
        if sequence <= last {
            prune(dir);
            return Err(HttpError::new(
                409,
                format!("sequence {sequence} is not after {last}: an old push cannot be replayed"),
            ));
        }
        git(
            Some(dir),
            &["update-ref", &format!("refs/keys/{reader_key}"), &tree],
        )
        .map_err(|e| HttpError::new(500, e))?;
        std::fs::write(sequence_dir(dir).join(&reader_key), format!("{sequence}\n"))
            .map_err(|e| HttpError::new(500, format!("recording the sequence: {e}")))?;
    }
    // Always, so a refused push leaves nothing behind either.
    prune(dir);
    if !verified {
        return Err(HttpError::new(403, "signature does not verify"));
    }
    eprintln!("secrets: {reader_key} now holds tree {tree}");
    Ok(format!("{tree}\n").into_bytes())
}

/// The secrets under each SecretReaderKey, merged, with the tree each key
/// resolved to. A name under two keys is an error; an unknown key holds
/// nothing.
pub(crate) fn load(config: &Config, keys: &[String]) -> Result<(Vec<Stored>, Vec<String>), String> {
    let dir = &config.secrets_git;
    let mut merged: Vec<Stored> = Vec::new();
    let mut trees = Vec::new();
    for key in keys {
        if !fmt::is_reader_key(key) {
            return Err(format!("{key:?} is not a SecretReaderKey"));
        }
        let Some(tree) = resolve_key(dir, key)? else {
            continue;
        };
        for secret in read_tree(dir, &tree)? {
            if merged.iter().any(|s| s.name == secret.name) {
                return Err(format!(
                    "secret {:?} is defined under two keys",
                    secret.name
                ));
            }
            merged.push(secret);
        }
        trees.push(tree);
    }
    Ok((merged, trees))
}

fn parse_body(body: &[u8]) -> Result<Vec<(fmt::Spec, String)>, String> {
    let json: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("body is not JSON: {e}"))?;
    let object = json.as_object().ok_or("body is not a JSON object")?;
    let mut secrets = Vec::new();
    for (name, entry) in object {
        let field = |key: &str| {
            entry
                .get(key)
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("secret {name:?} has no {key}"))
        };
        let spec = fmt::parse_spec(name, field(fmt::PUSHED_SPEC)?)?;
        if spec.name != *name || spec.value.is_some() {
            return Err(format!(
                "secret {name:?}: a pushed spec names no name and no value"
            ));
        }
        secrets.push((spec, field(fmt::PUSHED_VALUE)?.to_string()));
    }
    Ok(secrets)
}

/// Store the push as `<name>/{spec,value}` and return the tree's oid.
fn write_tree(dir: &str, secrets: &[(fmt::Spec, String)]) -> Result<String, String> {
    use gix::objs::tree::{Entry, EntryKind};
    let repo = gix::open(dir).map_err(|e| format!("opening {dir}: {e}"))?;
    let blob = |bytes: &[u8]| {
        repo.write_blob(bytes)
            .map(|id| id.detach())
            .map_err(|e| format!("writing a blob: {e}"))
    };
    let tree = |mut entries: Vec<Entry>| {
        entries.sort();
        repo.write_object(gix::objs::Tree { entries })
            .map(|id| id.detach())
            .map_err(|e| format!("writing a tree: {e}"))
    };
    let mut top = Vec::new();
    for (spec, value) in secrets {
        let entries = vec![
            Entry {
                mode: EntryKind::Blob.into(),
                filename: fmt::PUSHED_SPEC.into(),
                oid: blob(spec.render_pushed().as_bytes())?,
            },
            Entry {
                mode: EntryKind::Blob.into(),
                filename: fmt::PUSHED_VALUE.into(),
                oid: blob(value.as_bytes())?,
            },
        ];
        top.push(Entry {
            mode: EntryKind::Tree.into(),
            filename: spec.name.as_str().into(),
            oid: tree(entries)?,
        });
    }
    Ok(tree(top)?.to_string())
}

fn read_tree(dir: &str, tree: &str) -> Result<Vec<Stored>, String> {
    let repo = gix::open(dir).map_err(|e| format!("opening {dir}: {e}"))?;
    let entries = |oid: gix::ObjectId| -> Result<Vec<(String, gix::ObjectId)>, String> {
        let object = repo
            .find_object(oid)
            .map_err(|e| format!("secret tree {oid}: {e}"))?;
        let tree = object
            .try_into_tree()
            .map_err(|e| format!("secret tree {oid}: {e}"))?;
        let decoded = tree
            .decode()
            .map_err(|e| format!("secret tree {oid}: {e}"))?;
        Ok(decoded
            .entries
            .iter()
            .map(|e| (e.filename.to_string(), e.oid.to_owned()))
            .collect())
    };
    let blob = |oid: gix::ObjectId| -> Result<String, String> {
        let object = repo
            .find_object(oid)
            .map_err(|e| format!("secret blob {oid}: {e}"))?;
        String::from_utf8(object.data.clone()).map_err(|e| format!("secret blob {oid}: {e}"))
    };
    let root = gix::ObjectId::from_hex(tree.as_bytes()).map_err(|e| format!("{tree}: {e}"))?;
    let mut out = Vec::new();
    for (name, oid) in entries(root)? {
        let parts = entries(oid)?;
        let part = |key: &str| -> Result<gix::ObjectId, String> {
            parts
                .iter()
                .find(|(n, _)| n == key)
                .map(|(_, oid)| *oid)
                .ok_or_else(|| format!("secret {name:?} has no {key}"))
        };
        let spec = fmt::parse_spec(&name, &blob(part(fmt::PUSHED_SPEC)?)?)?;
        out.push(Stored {
            name: name.clone(),
            entropy: spec.entropy.unwrap_or_default(),
            value: blob(part(fmt::PUSHED_VALUE)?)?,
            readers: spec.readers,
        });
    }
    Ok(out)
}

fn resolve_key(dir: &str, key: &str) -> Result<Option<String>, String> {
    let output = Command::new("git")
        .args(["-C", dir, "rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/keys/{key}^{{tree}}"))
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Some(1) => Ok(None),
        _ => Err(format!(
            "resolving refs/keys/{key}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

fn sequence_dir(dir: &str) -> PathBuf {
    Path::new(dir).join("caos-sequence")
}

fn last_sequence(dir: &str, key: &str) -> Result<u64, String> {
    match std::fs::read_to_string(sequence_dir(dir).join(key)) {
        Ok(text) => text
            .trim()
            .parse()
            .map_err(|_| format!("corrupt sequence for {key}: {text:?}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(format!("reading the sequence for {key}: {e}")),
    }
}

/// Drop every object no key's ref reaches. A failure only delays it to the
/// next push, so it is logged rather than returned.
fn prune(dir: &str) {
    if let Err(e) = git(Some(dir), &["prune", "--expire=now"]) {
        eprintln!("secrets: prune failed: {e}");
    }
}

fn git(dir: Option<&str>, args: &[&str]) -> Result<(), String> {
    let mut command = Command::new("git");
    if let Some(dir) = dir {
        command.args(["-C", dir]);
    }
    let output = command
        .args(args)
        .output()
        .map_err(|e| format!("running git {args:?}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, readers: &str) -> (fmt::Spec, String) {
        let spec = fmt::parse_spec(name, &format!("entropy=0123456789abcdef\n{readers}")).unwrap();
        (spec, format!("value-of-{name}"))
    }

    #[test]
    fn a_written_tree_reads_back_and_prunes_what_it_replaced() {
        let tmp = std::env::temp_dir().join(format!(
            "caos-secret-store-{}-{:?}",
            std::process::id(),
            std::time::Instant::now()
        ));
        let dir = tmp.join("secrets.git");
        let dir = dir.to_str().unwrap();
        init(dir).unwrap();
        let key = "a".repeat(64);

        let first = write_tree(dir, &[spec("gh", "reader:@=x conversation=c\n")]).unwrap();
        git(
            Some(dir),
            &["update-ref", &format!("refs/keys/{key}"), &first],
        )
        .unwrap();
        let read = read_tree(dir, &first).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].value, "value-of-gh");
        assert_eq!(read[0].entropy, "0123456789abcdef");

        let second = write_tree(dir, &[spec("npm", "")]).unwrap();
        git(
            Some(dir),
            &["update-ref", &format!("refs/keys/{key}"), &second],
        )
        .unwrap();
        prune(dir);
        assert!(
            read_tree(dir, &first).is_err(),
            "the replaced tree must be gone"
        );
        assert_eq!(resolve_key(dir, &key).unwrap(), Some(second));
        assert_eq!(resolve_key(dir, &"b".repeat(64)).unwrap(), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
