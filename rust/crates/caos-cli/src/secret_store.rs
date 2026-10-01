//! `secrets-init` and `secrets-push`: the user's side of the secret store
//! (SPEC.md, "Secrets"). The local directory is the only copy; the server never
//! returns a value.

use std::path::{Path, PathBuf};

use caos_world::secrets::{self as fmt, Spec, Value};
use ed25519_dalek::{Signer, SigningKey};

/// The SecretWriterKey, inside the directory it signs for. A dotfile, so it is
/// never pushed.
const WRITER_KEY_FILE: &str = ".secret-writer-key";

/// Below this an entropy is guessable out of its `secret-hash`.
const MIN_ENTROPY_LEN: usize = 16;

/// `secrets-init [--dir=<d>]`: create the directory and its key pair, and
/// print the SecretReaderKey.
pub fn cli_secrets_init(args: &[String]) -> Result<(), String> {
    let (dir, server) = parse_flags(args)?;
    if server.is_some() {
        return Err("secrets-init takes no --server".to_string());
    }
    let key_path = dir.join(WRITER_KEY_FILE);
    if key_path.exists() {
        let key = read_writer_key(&dir)?;
        println!("{}", hex(&key.verifying_key().to_bytes()));
        return Err(format!("{} already exists", key_path.display()));
    }
    create_private_dir(&dir)?;
    let seed: [u8; 32] = random_bytes()?;
    write_private(&key_path, format!("{}\n", hex(&seed)).as_bytes())?;
    println!(
        "{}",
        hex(&SigningKey::from_bytes(&seed).verifying_key().to_bytes())
    );
    Ok(())
}

/// `secrets-push [--dir=<d>] [--server=<url>]`: replace the server's tree for
/// this key with the directory's secrets.
pub fn cli_secrets_push(args: &[String]) -> Result<(), String> {
    let (dir, server) = parse_flags(args)?;
    let key = read_writer_key(&dir)?;
    let reader_key = hex(&key.verifying_key().to_bytes());
    let secrets = load_dir(&dir)?;

    let tree = tree_oid(&secrets)?;
    let sequence = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("reading the clock: {e}"))?
        .as_millis() as u64;
    let signature = key.sign(&fmt::push_message(&reader_key, &tree, sequence));

    let mut body = serde_json::Map::new();
    for (spec, value) in &secrets {
        body.insert(
            spec.name.clone(),
            serde_json::json!({ fmt::PUSHED_SPEC: spec.render_pushed(), fmt::PUSHED_VALUE: value }),
        );
    }
    let body = serde_json::Value::Object(body).to_string();
    let server = match server {
        Some(server) => server,
        None => caos::GitTransport::from_cwd()
            .and_then(|t| caos::Transport::server_url(&t))
            .map_err(|e| format!("no --server, and no caos remote here: {e}"))?,
    };
    let headers = [
        (fmt::PUSH_KEY_HEADER, reader_key.clone()),
        (fmt::PUSH_SEQUENCE_HEADER, sequence.to_string()),
        (fmt::PUSH_SIGNATURE_HEADER, hex(&signature.to_bytes())),
    ];
    let response = caos::server_request(
        &server,
        &caos::ServerRequest {
            method: "POST",
            path: "/secrets/push",
            headers: &headers,
            body: Some(body.as_bytes()),
            timeout_secs: Some(60),
        },
    )?;
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "push refused: {} {}: {}",
            response.status,
            response.reason,
            String::from_utf8_lossy(&response.body).trim()
        ));
    }
    let names: Vec<&str> = secrets.iter().map(|(s, _)| s.name.as_str()).collect();
    println!(
        "pushed {} secret(s) for {reader_key}: {}",
        names.len(),
        names.join(" ")
    );
    Ok(())
}

fn parse_flags(args: &[String]) -> Result<(PathBuf, Option<String>), String> {
    let mut dir = None;
    let mut server = None;
    for arg in args {
        if let Some(d) = arg.strip_prefix("--dir=") {
            dir = Some(PathBuf::from(d));
        } else if let Some(s) = arg.strip_prefix("--server=") {
            server = Some(s.to_string());
        } else {
            return Err(format!("unknown argument {arg:?}"));
        }
    }
    let dir = match dir {
        Some(dir) => dir,
        None => default_dir()?,
    };
    Ok((dir, server))
}

/// The SecretReaderKeys this process presents, space-separated in the first
/// of: the checkout's `caos.secret-readers` (a cloud session's setup line puts
/// them there); the default directory's own key.
pub fn reader_keys() -> Vec<String> {
    if let Ok(out) = std::process::Command::new("git")
        .args(["config", "--get", "caos.secret-readers"])
        .stderr(std::process::Stdio::null())
        .output()
    {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
        }
    }
    default_dir()
        .and_then(|dir| read_writer_key(&dir))
        .map(|key| vec![hex(&key.verifying_key().to_bytes())])
        .unwrap_or_default()
}

/// `$CAOS_SECRETS_DIR`, else `$XDG_CONFIG_HOME/caos/secrets`, else
/// `~/.config/caos/secrets`.
fn default_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("CAOS_SECRETS_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(config) => PathBuf::from(config),
        None => PathBuf::from(
            std::env::var_os("HOME").ok_or("neither --dir, $CAOS_SECRETS_DIR nor $HOME is set")?,
        )
        .join(".config"),
    };
    Ok(config.join("caos").join("secrets"))
}

fn read_writer_key(dir: &Path) -> Result<SigningKey, String> {
    let path = dir.join(WRITER_KEY_FILE);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "reading {}: {e} (run secrets-init to create one)",
            path.display()
        )
    })?;
    let seed: [u8; 32] = unhex(text.trim())
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| format!("{} is not 64 hex characters", path.display()))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Every secret in `dir`, with its value resolved, sorted by name. A missing
/// entropy is generated and written back, so it survives the next push.
fn load_dir(dir: &Path) -> Result<Vec<(Spec, String)>, String> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))? {
        let path = entry
            .map_err(|e| format!("reading {}: {e}", dir.display()))?
            .path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if !name.starts_with('.') && path.is_file() {
            files.push((name, path));
        }
    }
    files.sort();
    let mut secrets: Vec<(Spec, String)> = Vec::new();
    for (file_name, path) in files {
        let mut text =
            std::fs::read_to_string(&path).map_err(|e| format!("reading {file_name}: {e}"))?;
        let mut spec = fmt::parse_spec(&file_name, &text)?;
        match &spec.entropy {
            None => {
                let entropy = hex(&random_bytes::<16>()?);
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&format!("entropy={entropy}\n"));
                std::fs::write(&path, &text).map_err(|e| format!("writing {file_name}: {e}"))?;
                eprintln!("{file_name}: added entropy");
                spec.entropy = Some(entropy);
            }
            Some(entropy) if entropy.len() < MIN_ENTROPY_LEN => {
                return Err(format!(
                    "{file_name}: weak entropy ({} chars < {MIN_ENTROPY_LEN})",
                    entropy.len()
                ))
            }
            Some(_) => {}
        }
        let value = match spec.value.take() {
            None => return Err(format!("secret {file_name}: no value= line")),
            Some(Value::Inline(value)) => value,
            Some(Value::File(rel)) => {
                let bytes = std::fs::read(dir.join(&rel))
                    .map_err(|e| format!("secret {file_name} value:@={rel}: {e}"))?;
                String::from_utf8(bytes)
                    .map_err(|e| format!("secret {file_name} value not UTF-8: {e}"))?
            }
        };
        if let Some((other, _)) = secrets.iter().find(|(s, _)| s.name == spec.name) {
            return Err(format!("two files declare the secret {:?}", other.name));
        }
        secrets.push((spec, value));
    }
    secrets.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    Ok(secrets)
}

/// The git tree the server will build from this push: `<name>/{spec,value}`.
/// Computed, not stored — nothing here may reach an object database.
fn tree_oid(secrets: &[(Spec, String)]) -> Result<String, String> {
    use gix::objs::tree::{Entry, EntryKind};
    let blob = |bytes: &[u8]| {
        gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
            .map_err(|e| format!("hashing: {e}"))
    };
    let tree = |mut entries: Vec<Entry>| -> Result<gix::ObjectId, String> {
        use gix::objs::WriteTo;
        entries.sort();
        let mut data = Vec::new();
        gix::objs::Tree { entries }
            .write_to(&mut data)
            .map_err(|e| format!("encoding tree: {e}"))?;
        gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Tree, &data)
            .map_err(|e| format!("hashing: {e}"))
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

fn create_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("chmod {}: {e}", dir.display()))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("creating {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
    use std::io::Read;
    let mut buf = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| format!("reading /dev/urandom: {e}"))?;
    Ok(buf)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_fills_entropy_and_resolves_values() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gh"), "value:@=gh.tok\n").unwrap();
        std::fs::write(dir.path().join("gh.tok"), "t0k").unwrap();
        std::fs::write(dir.path().join(".ignored"), "not a secret").unwrap();
        // `gh.tok` is itself a file in the directory, so it must parse as a
        // secret too — which it does not. Keep value files under a subdir.
        assert!(load_dir(dir.path()).is_err());

        std::fs::remove_file(dir.path().join("gh.tok")).unwrap();
        std::fs::create_dir(dir.path().join("values")).unwrap();
        std::fs::write(dir.path().join("values/gh"), "t0k").unwrap();
        std::fs::write(dir.path().join("gh"), "value:@=values/gh\n").unwrap();
        let secrets = load_dir(dir.path()).unwrap();
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].1, "t0k");
        let written = std::fs::read_to_string(dir.path().join("gh")).unwrap();
        assert!(written.contains("entropy="), "{written}");
        // Stable: the second load reads the entropy it wrote.
        assert_eq!(tree_oid(&secrets), tree_oid(&load_dir(dir.path()).unwrap()));
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(unhex(&hex(&[0, 1, 254, 255])), Some(vec![0, 1, 254, 255]));
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex("abc"), None);
    }
}
