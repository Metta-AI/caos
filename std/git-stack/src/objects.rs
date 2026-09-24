//! Individual Git objects over the server API; no checkout or closure fetch.
use conversation_protocol::v3::tree::{
    encode_commit_bytes, encode_tree_bytes, is_canonical_tree_order, parse_commit_bytes,
    parse_tree_bytes,
};
use conversation_protocol::v3::{CommitInfo, ObjectStore, Oid, Signature, StoreError, TreeEntry};
use std::cell::RefCell;
use std::collections::HashMap;

pub struct RemoteStore {
    server: String,
    objects: RefCell<HashMap<Oid, (String, Vec<u8>)>>,
}

impl RemoteStore {
    pub fn new(server: String) -> Self {
        Self {
            server: server.trim_end_matches('/').to_owned(),
            objects: RefCell::new(HashMap::new()),
        }
    }

    pub fn from_env() -> Result<Self, String> {
        let server = std::env::var("CAOS_SERVER_URL").map_err(|_| "CAOS_SERVER_URL not set")?;
        Ok(Self::new(server))
    }

    fn read(&self, oid: &Oid, expected: &'static str) -> Result<Vec<u8>, StoreError> {
        if !self.objects.borrow().contains_key(oid) {
            let response = minreq::get(format!("{}/object/{oid}", self.server))
                .with_timeout(60)
                .send()
                .map_err(|e| StoreError::Other(format!("reading object {oid}: {e}")))?;
            match response.status_code {
                200 => (),
                404 => return Err(StoreError::Missing(oid.clone())),
                status => {
                    return Err(StoreError::Other(format!(
                        "reading object {oid}: HTTP {status}"
                    )))
                }
            }
            let object = decode_object(response.as_bytes())?;
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
        let mut body = format!("{kind} {}\0", bytes.len()).into_bytes();
        body.extend_from_slice(bytes);
        let response = minreq::post(format!("{}/object/", self.server))
            .with_timeout(60)
            .with_body(body)
            .send()
            .map_err(|e| StoreError::Other(format!("writing {kind} object: {e}")))?;
        if response.status_code != 200 {
            return Err(StoreError::Other(format!(
                "writing {kind} object: HTTP {}: {}",
                response.status_code,
                String::from_utf8_lossy(response.as_bytes())
            )));
        }
        let text = std::str::from_utf8(response.as_bytes())
            .map_err(|_| StoreError::Other("object response is not UTF-8".into()))?;
        let oid = Oid::parse(text.trim(), "stored object").map_err(StoreError::Other)?;
        self.objects
            .get_mut()
            .insert(oid.clone(), (kind.into(), bytes.to_vec()));
        Ok(oid)
    }
}

fn decode_object(raw: &[u8]) -> Result<(String, Vec<u8>), StoreError> {
    let invalid = || StoreError::Other("invalid serialized object response".into());
    let end = raw.iter().position(|byte| *byte == 0).ok_or_else(invalid)?;
    let header = std::str::from_utf8(&raw[..end]).map_err(|_| invalid())?;
    let (kind, size) = header.split_once(' ').ok_or_else(invalid)?;
    if !matches!(kind, "blob" | "tree" | "commit")
        || size.parse::<usize>().ok() != Some(raw.len() - end - 1)
    {
        return Err(invalid());
    }
    Ok((kind.into(), raw[end + 1..].to_vec()))
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
    fn object_envelope_preserves_raw_bytes_and_rejects_bad_lengths() {
        assert_eq!(
            decode_object(b"blob 3\0a\0\xff").unwrap(),
            ("blob".into(), vec![b'a', 0, 255])
        );
        for bad in [
            &b"blob 4\0abc"[..],
            b"blob 2\0abc",
            b"blob three\0abc",
            b"tree 0",
            b"tag 0\0",
        ] {
            assert!(decode_object(bad).is_err());
        }
    }

    #[test]
    fn commit_serialization_preserves_raw_messages_and_empty_tree() {
        let tree = conversation_protocol::v3::oid::empty_tree();
        assert_eq!(decode_object(b"tree 0\0").unwrap(), ("tree".into(), vec![]));
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
