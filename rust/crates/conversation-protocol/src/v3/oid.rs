use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::tree::{CommitInfo, ObjectStore, Signature};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid(String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObjectKind {
    Blob,
    Tree,
    Commit,
}

impl ObjectKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Blob => "blob",
            ObjectKind::Tree => "tree",
            ObjectKind::Commit => "commit",
        }
    }
}

impl Oid {
    pub fn parse(value: &str, what: &str) -> Result<Oid, String> {
        if value.len() != 40 || !is_lower_hex(value) {
            return Err(format!(
                "{what} must be a lowercase 40-character hexadecimal hash, got {value:?}"
            ));
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn encode_line(&self) -> Vec<u8> {
        format!("{self}\n").into_bytes()
    }

    pub fn parse_line(bytes: &[u8], what: &str) -> Result<Oid, String> {
        if bytes.len() != 41 || bytes.last() != Some(&b'\n') {
            return Err(format!(
                "{what} must be exactly 40 lowercase hexadecimal characters plus LF"
            ));
        }
        let text =
            std::str::from_utf8(&bytes[..40]).map_err(|_| format!("{what} must be UTF-8"))?;
        Oid::parse(text, what)
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl AsRef<str> for Oid {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Serialize for Oid {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Oid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Oid::parse(&value, "oid").map_err(serde::de::Error::custom)
    }
}

pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
pub const G3: &str = "a2519b3360c5b1ded9a8cb7e5869d32901eae743";
pub const G3_MESSAGE: &str = "caos-conversation-genesis-v3\n";

pub fn empty_tree() -> Oid {
    Oid::parse(EMPTY_TREE, "empty tree").expect("EMPTY_TREE is a valid oid")
}

pub fn g3() -> Oid {
    Oid::parse(G3, "v3 genesis").expect("G3 is a valid oid")
}

pub fn genesis_commit() -> CommitInfo {
    let signature = Signature {
        name: "caos".to_string(),
        email: "caos@caos".to_string(),
        time: 0,
        offset: "+0000".to_string(),
    };
    CommitInfo {
        tree: empty_tree(),
        parents: Vec::new(),
        author: signature.clone(),
        committer: signature,
        message: G3_MESSAGE.as_bytes().to_vec(),
    }
}

pub fn ensure_genesis(store: &mut dyn ObjectStore) -> Result<Oid, String> {
    let minted = store
        .write_commit(&genesis_commit())
        .map_err(String::from)?;
    if minted.as_str() != G3 {
        return Err(format!(
            "genesis oid mismatch: minted {minted}, expected {G3}"
        ));
    }
    Ok(minted)
}

pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

pub(crate) fn object_id(kind: ObjectKind, bytes: &[u8]) -> Oid {
    use sha1::{Digest, Sha1};

    let mut hasher = Sha1::new();
    hasher.update(format!("{} {}\0", kind.as_str(), bytes.len()).as_bytes());
    hasher.update(bytes);
    Oid::parse(&hex_lower(&hasher.finalize()), "minted object").expect("SHA-1 produces a valid oid")
}

pub(crate) fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

pub(crate) fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v3::tree::MemoryStore;

    #[test]
    fn oids_are_canonical_lowercase_sha1s() {
        let valid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(Oid::parse(valid, "object").unwrap().as_str(), valid);
        for invalid in [
            "a",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "gggggggggggggggggggggggggggggggggggggggg",
        ] {
            assert!(Oid::parse(invalid, "object").is_err());
        }
    }

    #[test]
    fn memory_store_mints_git_known_answers() {
        let mut store = MemoryStore::new();
        assert_eq!(ensure_genesis(&mut store), Ok(g3()));
        assert!(store.contains(&g3()));
        assert_eq!(store.write_tree(&[]).unwrap(), empty_tree());
    }
}
