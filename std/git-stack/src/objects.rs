//! Individual Git objects over the server API; no checkout or closure fetch.
use conversation_protocol::v3::tree::{
    encode_commit_bytes, encode_tree_bytes, is_canonical_tree_order, parse_commit_bytes,
    parse_tree_bytes,
};
use conversation_protocol::v3::{CommitInfo, ObjectStore, Oid, Signature, StoreError, TreeEntry};
use std::cell::RefCell;
use std::collections::HashMap;

pub struct RemoteStore {
    transport: caos::HttpTransport,
    objects: RefCell<HashMap<Oid, (String, Vec<u8>)>>,
}

impl RemoteStore {
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            transport: caos::HttpTransport::from_env()?,
            objects: RefCell::new(HashMap::new()),
        })
    }

    fn read(&self, oid: &Oid, expected: &'static str) -> Result<Vec<u8>, StoreError> {
        use caos::Transport;
        if !self.objects.borrow().contains_key(oid) {
            let object = self
                .transport
                .get_object(oid.as_str())
                .map_err(StoreError::Other)?;
            self.objects.borrow_mut().insert(oid.clone(), object);
        }
        let objects = self.objects.borrow();
        let (kind, bytes) = &objects[oid];
        if kind != expected {
            return Err(StoreError::WrongType {
                oid: oid.clone(),
                expected,
            });
        }
        Ok(bytes.clone())
    }

    fn write(&mut self, kind: &str, bytes: &[u8]) -> Result<Oid, StoreError> {
        use caos::Transport;
        let id = self
            .transport
            .put_object(kind, bytes)
            .map_err(StoreError::Other)?;
        let oid = Oid::parse(&id.to_string(), "stored object").map_err(StoreError::Other)?;
        self.objects
            .get_mut()
            .insert(oid.clone(), (kind.into(), bytes.to_vec()));
        Ok(oid)
    }
}

impl ObjectStore for RemoteStore {
    fn read_blob(&self, oid: &Oid) -> Result<Vec<u8>, StoreError> {
        self.read(oid, "blob")
    }

    fn read_tree(&self, oid: &Oid) -> Result<Vec<TreeEntry>, StoreError> {
        parse_tree_bytes(oid, &self.read(oid, "tree")?)
    }

    fn read_commit(&self, oid: &Oid) -> Result<CommitInfo, StoreError> {
        parse_commit_bytes(oid, &self.read(oid, "commit")?)
    }

    fn write_blob(&mut self, bytes: &[u8]) -> Result<Oid, StoreError> {
        self.write("blob", bytes)
    }

    fn write_tree(&mut self, entries: &[TreeEntry]) -> Result<Oid, StoreError> {
        if !is_canonical_tree_order(entries) {
            return Err(StoreError::Other(
                "tree entries are not in canonical order".into(),
            ));
        }
        self.write("tree", &encode_tree_bytes(entries))
    }

    fn write_commit(&mut self, commit: &CommitInfo) -> Result<Oid, StoreError> {
        self.write("commit", &encode_commit_bytes(commit))
    }
}

pub fn parse_signature(value: &str) -> Result<Signature, String> {
    let invalid = || "signature must be Name <email> <unix-seconds> <+/-HHMM>".to_owned();
    if value.contains(['\n', '\r', '\0']) {
        return Err(invalid());
    }
    let (name, rest) = value.rsplit_once(" <").ok_or_else(invalid)?;
    let (email, rest) = rest.split_once("> ").ok_or_else(invalid)?;
    let (time, offset) = rest.split_once(' ').ok_or_else(invalid)?;
    if name.is_empty()
        || email.is_empty()
        || name.contains(['<', '>'])
        || email.contains(['<', '>'])
        || offset.len() != 5
        || !matches!(offset.as_bytes()[0], b'+' | b'-')
        || !offset.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(invalid());
    }
    Ok(Signature {
        name: name.into(),
        email: email.into(),
        time: time.parse().map_err(|_| invalid())?,
        offset: offset.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_serialization_preserves_raw_messages_and_empty_tree() {
        let tree = conversation_protocol::v3::oid::empty_tree();
        assert_eq!(encode_tree_bytes(&[]), Vec::<u8>::new());
        let author = parse_signature("A <a@example.com> 1 +0000").unwrap();
        let info = CommitInfo {
            tree: tree.clone(),
            parents: vec![],
            author: author.clone(),
            committer: author,
            extra_headers: b"encoding ISO-8859-1\n".to_vec(),
            message: b"caf\xe9\n".to_vec(),
        };
        let bytes = encode_commit_bytes(&info);
        assert_eq!(parse_commit_bytes(&tree, &bytes).unwrap(), info);
    }

    #[test]
    fn signatures_are_explicit_and_cannot_inject_headers() {
        let parsed = parse_signature("A Person <a@example.com> 123 -0530").unwrap();
        assert_eq!(parsed.name, "A Person");
        assert_eq!(parsed.time, 123);
        assert_eq!(parsed.offset, "-0530");
        for bad in [
            "A <a> 1",
            "A <a> now +0000",
            "A <a> 1 UTC",
            "A <a> 1 +0000\nparent bad",
        ] {
            assert!(parse_signature(bad).is_err());
        }
    }
}
