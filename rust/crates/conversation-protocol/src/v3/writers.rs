//! Ref writers (design/ref-writers.md): the formats a client, a worker and the
//! server's pre-receive hook must agree on. Signing and verifying live behind
//! the `ed25519` feature so std tools that only push with a run token do not
//! compile a signature crate.

use super::oid::{object_id, ObjectKind, Oid};
use super::tree::{
    encode_commit_bytes, encode_tree_bytes, CommitInfo, Mode, ObjectStore, Signature, TreeEntry,
};

pub const NAMESPACE_PREFIX: &str = "refs/caos/w/";
pub const WRITERS_REF: &str = "writers";
pub const WRITERS_PATH: &str = ".caos/writers";
/// The push option carrying a write's proof: `caos-auth=sig:…` or `caos-auth=run:…`.
pub const PUSH_OPTION: &str = "caos-auth";
/// Where the server injects a job's run token (`/secret/<this>`).
pub const TOKEN_SECRET: &str = "caos-write";
pub const TOKEN_PATH: &str = "/secret/caos-write";
/// The request header that gives a top-level request its writer's authority.
pub const ADMIT_HEADER: &str = "X-Caos-Write";
/// The ArgTree arg naming the namespaces a job asks to write.
pub const WRITES_ARG: &str = "writes";
/// How long a client signature stays valid.
pub const SIGNATURE_TTL_SECS: u64 = 600;
pub const ZERO_OID: &str = "0000000000000000000000000000000000000000";

pub fn writers_ref(namespace: &str) -> String {
    format!("{NAMESPACE_PREFIX}{namespace}/{WRITERS_REF}")
}

/// `refs/caos/w/<ns>/<rest>` → `(ns, rest)`, for a well-formed namespace id.
pub fn split_ref(refname: &str) -> Option<(&str, &str)> {
    let (namespace, rest) = refname.strip_prefix(NAMESPACE_PREFIX)?.split_once('/')?;
    (is_namespace(namespace) && !rest.is_empty()).then_some((namespace, rest))
}

pub fn is_namespace(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn validate_namespace(value: &str) -> Result<(), String> {
    if is_namespace(value) {
        Ok(())
    } else {
        Err(format!(
            "{value:?} is not a namespace id (40 lowercase hex)"
        ))
    }
}

pub fn is_key(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Writer {
    pub key: String,
    pub label: String,
}

/// `.caos/writers`: one `<key> [label]` per line; `#` starts a comment.
pub fn parse_writers(text: &str) -> Result<Vec<Writer>, String> {
    let mut writers: Vec<Writer> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, label) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        if !is_key(key) {
            return Err(format!(
                "{WRITERS_PATH}: {key:?} is not an ed25519 public key"
            ));
        }
        if writers.iter().any(|w| w.key == key) {
            return Err(format!("{WRITERS_PATH}: {key} is listed twice"));
        }
        writers.push(Writer {
            key: key.to_string(),
            label: label.trim().to_string(),
        });
    }
    Ok(writers)
}

/// A list holding one writer, `key`.
pub fn sole_writer(key: &str) -> Vec<Writer> {
    vec![Writer {
        key: key.to_string(),
        label: String::new(),
    }]
}

pub fn format_writers(writers: &[Writer]) -> String {
    let mut text = String::from("# <ed25519 public key>  <label, display only>\n");
    for writer in writers {
        if writer.label.is_empty() {
            text.push_str(&format!("{}\n", writer.key));
        } else {
            text.push_str(&format!("{}  {}\n", writer.key, writer.label));
        }
    }
    text
}

/// One ref update as the hook sees it: absent is the zero oid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub old: String,
    pub new: String,
    pub refname: String,
}

impl Command {
    pub fn new(refname: &str, old: Option<&str>, new: Option<&str>) -> Command {
        Command {
            old: old.unwrap_or(ZERO_OID).to_string(),
            new: new.unwrap_or(ZERO_OID).to_string(),
            refname: refname.to_string(),
        }
    }
}

/// What a client signs: the expiry and every command, sorted by ref.
pub fn update_message(expiry: u64, commands: &[Command]) -> Vec<u8> {
    let mut sorted: Vec<&Command> = commands.iter().collect();
    sorted.sort_by(|a, b| a.refname.cmp(&b.refname));
    let mut text = format!("caos-ref-update v1\n{expiry}\n");
    for c in sorted {
        text.push_str(&format!("{} {} {}\n", c.old, c.new, c.refname));
    }
    text.into_bytes()
}

/// What a client signs to give a top-level request its authority.
pub fn admit_message(expiry: u64, method: &str, target: &str) -> Vec<u8> {
    format!("caos-write-admit v1\n{expiry}\n{method} {target}\n").into_bytes()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    Signed {
        key: String,
        expiry: u64,
        signature: String,
    },
    Run {
        token: String,
    },
}

impl Auth {
    /// The push option, `caos-auth=…`.
    pub fn option(&self) -> String {
        match self {
            Auth::Signed {
                key,
                expiry,
                signature,
            } => format!("{PUSH_OPTION}=sig:{key}:{expiry}:{signature}"),
            Auth::Run { token } => format!("{PUSH_OPTION}=run:{token}"),
        }
    }

    pub fn parse_option(option: &str) -> Option<Result<Auth, String>> {
        let value = option.strip_prefix(PUSH_OPTION)?.strip_prefix('=')?;
        Some(Auth::parse(value))
    }

    fn parse(value: &str) -> Result<Auth, String> {
        if let Some(token) = value.strip_prefix("run:") {
            if token.is_empty() || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("malformed run token".to_string());
            }
            return Ok(Auth::Run {
                token: token.to_string(),
            });
        }
        let signed = value
            .strip_prefix("sig:")
            .ok_or_else(|| format!("unknown {PUSH_OPTION} form"))?;
        let mut parts = signed.splitn(3, ':');
        let (Some(key), Some(expiry), Some(signature)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(format!("{PUSH_OPTION}=sig: needs key:expiry:signature"));
        };
        if !is_key(key) {
            return Err(format!("{key:?} is not an ed25519 public key"));
        }
        Ok(Auth::Signed {
            key: key.to_string(),
            expiry: expiry
                .parse()
                .map_err(|_| format!("bad signature expiry {expiry:?}"))?,
            signature: signature.to_string(),
        })
    }
}

/// `X-Caos-Write: <key> <expiry> <signature>`.
pub fn admit_header(key: &str, expiry: u64, signature: &str) -> String {
    format!("{key} {expiry} {signature}")
}

pub fn parse_admit_header(value: &str) -> Result<(String, u64, String), String> {
    let mut parts = value.split_whitespace();
    let (Some(key), Some(expiry), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(format!("{ADMIT_HEADER} needs <key> <expiry> <signature>"));
    };
    if !is_key(key) {
        return Err(format!(
            "{ADMIT_HEADER}: {key:?} is not an ed25519 public key"
        ));
    }
    let expiry = expiry
        .parse()
        .map_err(|_| format!("{ADMIT_HEADER}: bad expiry {expiry:?}"))?;
    Ok((key.to_string(), expiry, signature.to_string()))
}

/// What a job's `writes` arg asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Writes {
    /// `*`: everything its creator was handed. For a job that only passes
    /// writes on, such as a test harness above the step that pushes.
    All,
    Namespaces(Vec<String>),
}

/// The `writes` arg: namespace ids separated by whitespace, or `*`.
pub fn parse_writes(value: &str) -> Result<Writes, String> {
    if value.trim() == "*" {
        return Ok(Writes::All);
    }
    let mut namespaces = Vec::new();
    for namespace in value.split_whitespace() {
        validate_namespace(namespace)?;
        if !namespaces.iter().any(|n| n == namespace) {
            namespaces.push(namespace.to_string());
        }
    }
    Ok(Writes::Namespaces(namespaces))
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

const GENESIS_SIGNATURE: &str = "caos";

/// A namespace's first `writers` commit. Its oid is the namespace id, so the
/// id is fixed by the initial list: nobody can claim a namespace before its
/// creator, or create one its creator is not in. The commit is deterministic
/// (`label` included), so the same writers and label always name the same
/// namespace.
pub fn genesis_commit(
    store: &mut dyn ObjectStore,
    writers: &[Writer],
    label: &str,
) -> Result<Oid, String> {
    let (blob, info, id) = genesis_objects(writers, label);
    write_writers_tree(store, &blob)?;
    let oid = store
        .write_commit(&info)
        .map_err(|e| format!("writing the namespace's first commit: {e:?}"))?;
    debug_assert_eq!(oid, id);
    Ok(oid)
}

fn write_writers_tree(store: &mut dyn ObjectStore, blob: &[u8]) -> Result<Oid, String> {
    let blob = store
        .write_blob(blob)
        .map_err(|e| format!("writing {WRITERS_PATH}: {e:?}"))?;
    let caos = store
        .write_tree(&[TreeEntry {
            name: "writers".to_string(),
            mode: Mode::Blob,
            oid: blob,
        }])
        .map_err(|e| format!("writing .caos: {e:?}"))?;
    store
        .write_tree(&[TreeEntry {
            name: ".caos".to_string(),
            mode: Mode::Tree,
            oid: caos,
        }])
        .map_err(|e| format!("writing the writers tree: {e:?}"))
}

/// The namespace id `genesis_commit` would write, without writing anything.
pub fn genesis_id(writers: &[Writer], label: &str) -> Oid {
    genesis_objects(writers, label).2
}

fn genesis_objects(writers: &[Writer], label: &str) -> (Vec<u8>, CommitInfo, Oid) {
    let blob = format_writers(writers).into_bytes();
    let blob_oid = object_id(ObjectKind::Blob, &blob);
    let caos = object_id(
        ObjectKind::Tree,
        &encode_tree_bytes(&[TreeEntry {
            name: "writers".to_string(),
            mode: Mode::Blob,
            oid: blob_oid,
        }]),
    );
    let root = object_id(
        ObjectKind::Tree,
        &encode_tree_bytes(&[TreeEntry {
            name: ".caos".to_string(),
            mode: Mode::Tree,
            oid: caos,
        }]),
    );
    let signature = Signature {
        name: GENESIS_SIGNATURE.to_string(),
        email: GENESIS_SIGNATURE.to_string(),
        time: 0,
        offset: "+0000".to_string(),
    };
    let commit = CommitInfo {
        tree: root,
        parents: Vec::new(),
        author: signature.clone(),
        committer: signature,
        extra_headers: Vec::new(),
        message: format!("caos namespace\n\n{label}\n").into_bytes(),
    };
    let oid = object_id(ObjectKind::Commit, &encode_commit_bytes(&commit));
    (blob, commit, oid)
}

/// A later `writers` commit: the new list on top of `parent`.
pub fn writers_commit(
    store: &mut dyn ObjectStore,
    parent: &Oid,
    writers: &[Writer],
    author: &Signature,
    message: &str,
) -> Result<Oid, String> {
    let root = write_writers_tree(store, format_writers(writers).as_bytes())?;
    store
        .write_commit(&CommitInfo {
            tree: root,
            parents: vec![parent.clone()],
            author: author.clone(),
            committer: author.clone(),
            extra_headers: Vec::new(),
            message: format!("{}\n", message.trim_end()).into_bytes(),
        })
        .map_err(|e| format!("writing the writers commit: {e:?}"))
}

/// The writers listed at `.caos/writers` in a `writers` commit.
pub fn read_writers(store: &dyn ObjectStore, commit: &Oid) -> Result<Vec<Writer>, String> {
    let info = store
        .read_commit(commit)
        .map_err(|e| format!("reading writers commit {commit}: {e:?}"))?;
    let caos = store
        .read_tree(&info.tree)
        .map_err(|e| format!("reading writers tree: {e:?}"))?
        .into_iter()
        .find(|e| e.name == ".caos" && e.mode == Mode::Tree)
        .ok_or_else(|| format!("writers commit {commit} has no .caos/"))?;
    let blob = store
        .read_tree(&caos.oid)
        .map_err(|e| format!("reading .caos: {e:?}"))?
        .into_iter()
        .find(|e| e.name == "writers" && e.mode == Mode::Blob)
        .ok_or_else(|| format!("writers commit {commit} has no {WRITERS_PATH}"))?;
    let bytes = store
        .read_blob(&blob.oid)
        .map_err(|e| format!("reading {WRITERS_PATH}: {e:?}"))?;
    parse_writers(&String::from_utf8(bytes).map_err(|_| format!("{WRITERS_PATH} is not UTF-8"))?)
}

/// A run token, if this process is a job the server granted one.
pub fn injected_token() -> Option<String> {
    std::fs::read_to_string(TOKEN_PATH)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

#[cfg(feature = "ed25519")]
pub use keys::*;

#[cfg(feature = "ed25519")]
mod keys {
    use super::super::oid::hex_lower;
    use super::*;
    use ed25519_dalek::{Signer, SigningKey, VerifyingKey};

    /// A writer key: the 32-byte seed, as 64 hex.
    pub struct WriterKey(SigningKey);

    impl WriterKey {
        pub fn parse(seed_hex: &str) -> Result<WriterKey, String> {
            let seed = unhex::<32>(seed_hex.trim())
                .ok_or_else(|| "a ref writer key is 64 hex characters".to_string())?;
            Ok(WriterKey(SigningKey::from_bytes(&seed)))
        }

        pub fn from_seed(seed: [u8; 32]) -> WriterKey {
            WriterKey(SigningKey::from_bytes(&seed))
        }

        pub fn seed_hex(&self) -> String {
            hex_lower(&self.0.to_bytes())
        }

        pub fn public(&self) -> String {
            hex_lower(&self.0.verifying_key().to_bytes())
        }

        pub fn sign(&self, message: &[u8]) -> String {
            hex_lower(&self.0.sign(message).to_bytes())
        }

        /// The push option proving `commands` were made by this key.
        pub fn sign_update(&self, commands: &[Command]) -> String {
            let expiry = now() + SIGNATURE_TTL_SECS;
            Auth::Signed {
                key: self.public(),
                expiry,
                signature: self.sign(&update_message(expiry, commands)),
            }
            .option()
        }

        /// The `X-Caos-Write` value for one request.
        pub fn sign_admission(&self, method: &str, target: &str) -> String {
            let expiry = now() + SIGNATURE_TTL_SECS;
            admit_header(
                &self.public(),
                expiry,
                &self.sign(&admit_message(expiry, method, target)),
            )
        }
    }

    pub fn verify(key: &str, message: &[u8], signature: &str) -> Result<(), String> {
        let key = unhex::<32>(key).ok_or_else(|| "malformed public key".to_string())?;
        let key =
            VerifyingKey::from_bytes(&key).map_err(|_| "not an ed25519 public key".to_string())?;
        let signature = unhex::<64>(signature).ok_or_else(|| "malformed signature".to_string())?;
        key.verify_strict(message, &ed25519_dalek::Signature::from_bytes(&signature))
            .map_err(|_| "signature does not verify".to_string())
    }

    fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
        if s.len() != N * 2 {
            return None;
        }
        let mut out = [0u8; N];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            out[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "3b6a27bcceb6a42d62a3a8d02a6f0d73653215771de243a63ac048a18b59da29";

    #[test]
    fn writers_round_trip_and_reject_junk() {
        let writers = vec![
            Writer {
                key: A.into(),
                label: "malcolm".into(),
            },
            Writer {
                key: "9".repeat(64),
                label: String::new(),
            },
        ];
        assert_eq!(parse_writers(&format_writers(&writers)).unwrap(), writers);
        assert!(parse_writers("nothex\n").is_err());
        assert!(parse_writers(&format!("{A}\n{A} again\n")).is_err());
    }

    #[test]
    fn refs_split_only_under_a_namespace() {
        let ns = "a".repeat(40);
        assert_eq!(split_ref(&writers_ref(&ns)), Some((ns.as_str(), "writers")));
        assert_eq!(split_ref("refs/caos/w/short/head"), None);
        assert_eq!(split_ref(&format!("refs/caos/w/{ns}/")), None);
        assert_eq!(split_ref("refs/heads/main"), None);
    }

    #[test]
    fn update_messages_ignore_command_order() {
        let a = Command::new("refs/b", None, Some(&"1".repeat(40)));
        let b = Command::new("refs/a", Some(&"2".repeat(40)), None);
        assert_eq!(
            update_message(5, &[a.clone(), b.clone()]),
            update_message(5, &[b, a])
        );
    }

    #[test]
    fn push_options_round_trip() {
        let signed = Auth::Signed {
            key: A.into(),
            expiry: 9,
            signature: "ab".into(),
        };
        assert_eq!(Auth::parse_option(&signed.option()), Some(Ok(signed)));
        let run = Auth::Run {
            token: "00ff".into(),
        };
        assert_eq!(Auth::parse_option(&run.option()), Some(Ok(run)));
        assert_eq!(Auth::parse_option("other=1"), None);
        assert!(matches!(
            Auth::parse_option("caos-auth=run:x y"),
            Some(Err(_))
        ));
    }

    #[test]
    fn genesis_is_deterministic_and_matches_what_it_writes() {
        let writers = vec![Writer {
            key: A.into(),
            label: String::new(),
        }];
        let mut store = crate::v3::MemoryStore::default();
        let written = genesis_commit(&mut store, &writers, "x").unwrap();
        assert_eq!(written, genesis_id(&writers, "x"));
        assert_ne!(written, genesis_id(&writers, "y"));
        assert_eq!(read_writers(&store, &written).unwrap(), writers);
    }

    #[cfg(feature = "ed25519")]
    #[test]
    fn signatures_verify_only_their_message() {
        let key = WriterKey::from_seed([7; 32]);
        let message = update_message(1, &[Command::new("refs/x", None, Some(&"1".repeat(40)))]);
        let signature = key.sign(&message);
        assert!(verify(&key.public(), &message, &signature).is_ok());
        assert!(verify(&key.public(), b"other", &signature).is_err());
    }
}
