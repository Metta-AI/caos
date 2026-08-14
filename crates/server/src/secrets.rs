//! Secrets: identity-is-capability injection (design/secrets.md).
//!
//! A secret is a value plus a set of *readers* — partial arg trees allowed to
//! see it. A job may read a secret iff its ArgTree is a **superset** of one of
//! the readers. The value never enters the ArgTree or the cache key, so
//! rotating it busts no cache and the value is never content-addressed.
//!
//! The store is **carried with the run as ephemeral context** (like the run
//! stack), not sourced on the server: the client reads its own git-ignored
//! `.caos-secrets`, resolves each reader with eval-path (the same evaluator the
//! run uses, so the resolved image oids match what the job carries — the server
//! must never eval), and sends the result in the `X-Caos-Secrets` header on
//! `GET /run`. The server parses it into [`Grant`]s, threads them through
//! promise resolution to every sub-run's dispatch, and at each dispatch does
//! the cheap subset-match + injection. So a sub-worker is entitled by matching
//! *its own* arg tree, never by inheritance (the no-delegation invariant).

use std::collections::{BTreeMap, HashSet};

/// The header carrying the serialized store from client to server.
pub(crate) const HEADER: &str = "X-Caos-Secrets";

/// One secret the run carries: its value, its entropy (the cache-isolation
/// capability — hashed into `secret-hash`, never stored raw), and the readers
/// (each a partial arg tree, already resolved client-side to name → oid).
#[derive(Clone)]
pub(crate) struct Grant {
    name: String,
    value: String,
    entropy: String,
    readers: Vec<BTreeMap<String, String>>,
}

/// Parse the `X-Caos-Secrets` header (JSON) into the carried grants. An
/// empty/absent header is no grants; a malformed one is logged and ignored
/// (fail closed — a parse error grants nothing).
pub(crate) fn parse_header(header: &str) -> Vec<Grant> {
    if header.trim().is_empty() {
        return Vec::new();
    }
    let value: serde_json::Value = match serde_json::from_str(header) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("secrets: ignoring malformed {HEADER} header: {e}");
            return Vec::new();
        }
    };
    let Some(array) = value.as_array() else {
        eprintln!("secrets: {HEADER} header is not a JSON array; ignoring");
        return Vec::new();
    };
    let mut grants = Vec::new();
    for entry in array {
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let value = entry.get("value").and_then(|v| v.as_str());
        let (Some(value), true) = (value, !name.is_empty()) else {
            eprintln!("secrets: skipping a grant missing name/value");
            continue;
        };
        let readers = entry
            .get("readers")
            .and_then(|v| v.as_array())
            .map(|rs| rs.iter().filter_map(reader_entries).collect())
            .unwrap_or_default();
        grants.push(Grant {
            name: name.to_string(),
            value: value.to_string(),
            entropy: entry
                .get("entropy")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            readers,
        });
    }
    grants
}

/// Parse one reader (a JSON object of name → oid string) into a partial arg
/// tree; `None` if it isn't an object of strings.
fn reader_entries(value: &serde_json::Value) -> Option<BTreeMap<String, String>> {
    let object = value.as_object()?;
    let mut entries = BTreeMap::new();
    for (name, oid) in object {
        entries.insert(name.clone(), oid.as_str()?.to_string());
    }
    Some(entries)
}

/// The secrets a job is entitled to: every carried grant with at least one
/// reader whose partial arg tree is a subset of the job's top-level arg entries
/// (`arg_entries`, as `compute::args_entries` reads them) — but ONLY if the arg
/// tree *already* carries the matching `secret-hash` (design/secrets.md). That
/// second condition proves the tree was produced by eval with this store, so a
/// value can never reach a worker whose cache key doesn't already reflect it
/// (injection ⟹ the isolating hash is in the key). A reader match without the
/// matching hash is refused, fail-closed. Returns (name, value), deduped by name.
pub(crate) fn grant(
    grants: &[Grant],
    arg_entries: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    // The hash this job's matched grants require. None ⇒ nothing matches.
    let Some(digest) = secret_hash(grants, arg_entries) else {
        return Vec::new();
    };
    // The `secret-hash` entry references a blob whose *content* is that digest,
    // so the entry's oid (what's in `arg_entries`) is the blob-hash of it.
    let expected = blob_oid(digest.as_bytes());
    match arg_entries.get(caos_world::SECRET_HASH_ARG) {
        Some(present) if *present == expected => {}
        present => {
            eprintln!(
                "secrets: refusing injection — {} is {present:?}, expected {expected} \
                 (worker not produced by eval with this store)",
                caos_world::SECRET_HASH_ARG
            );
            return Vec::new();
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for grant in grants {
        let visible = grant
            .readers
            .iter()
            .any(|reader| is_subset(reader, arg_entries));
        if visible && seen.insert(grant.name.clone()) {
            eprintln!("secret {}: granted to this job", grant.name);
            out.push((grant.name.clone(), grant.value.clone()));
        }
    }
    out
}

/// Is `reader` (a partial arg tree) a subset of `job`? Every (name, oid) the
/// reader pins must appear identically in the job — pure oid equality, the same
/// match the runner rendezvous uses. Extra job entries (salt, the job's own
/// call args, …) are wildcards.
fn is_subset(reader: &BTreeMap<String, String>, job: &BTreeMap<String, String>) -> bool {
    reader.iter().all(|(name, oid)| job.get(name) == Some(oid))
}

/// The `secret-hash` cache-isolation tag for a job with base arg entries
/// `arg_entries` (design/secrets.md): the git-blob digest of the
/// `(name, entropy)` pairs of the grants whose readers match — `None` when no
/// grant matches (a secret-free run stays globally shared). Whoever assembles
/// the ArgTree folds this in as the reserved `secret-hash` entry, so the entropy
/// itself never touches the tree. Client and server must agree, so both hash the
/// shared [`caos_world::secret_hash_material`].
pub(crate) fn secret_hash(
    grants: &[Grant],
    arg_entries: &BTreeMap<String, String>,
) -> Option<String> {
    let pairs: Vec<(&str, &str)> = grants
        .iter()
        .filter(|g| g.readers.iter().any(|r| is_subset(r, arg_entries)))
        .map(|g| (g.name.as_str(), g.entropy.as_str()))
        .collect();
    if pairs.is_empty() {
        return None;
    }
    let material = caos_world::secret_hash_material(&pairs);
    Some(blob_oid(&material))
}

/// The git-blob object id (hex) of `bytes` — computed, not stored. The
/// `secret-hash` digest and the tree-entry oid that references it are both blob
/// hashes, so client and server agree by using this one function shape (the
/// client's `hash_bytes` is its twin).
fn blob_oid(bytes: &[u8]) -> String {
    gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
        .expect("hashing bytes")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn subset_needs_every_pinned_entry() {
        let job = map(&[("base", "aa"), ("std", "bb"), ("worker1", "cc")]);
        assert!(is_subset(&map(&[("base", "aa")]), &job));
        assert!(is_subset(&map(&[("base", "aa"), ("worker1", "cc")]), &job));
        // Disagreeing pin, and a pin the job lacks, both fail.
        assert!(!is_subset(&map(&[("base", "zz")]), &job));
        assert!(!is_subset(&map(&[("base", "aa"), ("marker", "x")]), &job));
    }

    #[test]
    fn grant_requires_the_matching_secret_hash() {
        let grants = parse_header(
            r#"[{"name":"tok","value":"s3cr3t","entropy":"E","readers":[
                 {"base":"aa"},
                 {"base":"bb","repo":"cc"}
               ]}]"#,
        );
        // A worker that matches a reader but carries NO secret-hash is refused
        // (it wasn't produced by eval with this store).
        assert!(grant(&grants, &map(&[("base", "aa")])).is_empty());
        // With the matching secret-hash present, the value is injected. The
        // entry is the blob-oid of the digest (how it rides in a real tree).
        let mut job = map(&[("base", "aa"), ("salt", "z")]);
        let digest = secret_hash(&grants, &job).unwrap();
        job.insert(
            caos_world::SECRET_HASH_ARG.to_string(),
            blob_oid(digest.as_bytes()),
        );
        assert_eq!(
            grant(&grants, &job),
            vec![("tok".to_string(), "s3cr3t".to_string())]
        );
        // A wrong secret-hash is refused.
        let mut forged = map(&[("base", "aa")]);
        forged.insert(
            caos_world::SECRET_HASH_ARG.to_string(),
            "deadbeef".to_string(),
        );
        assert!(grant(&grants, &forged).is_empty());
        // A worker matching NO reader gets nothing (and needs no hash).
        assert!(grant(&grants, &map(&[("base", "xx")])).is_empty());
    }

    #[test]
    fn empty_or_malformed_header_grants_nothing() {
        assert!(parse_header("").is_empty());
        assert!(parse_header("   ").is_empty());
        assert!(parse_header("not json").is_empty());
        assert!(parse_header(r#"{"not":"an array"}"#).is_empty());
        // A grant missing its value is skipped.
        assert!(parse_header(r#"[{"name":"x"}]"#).is_empty());
    }

    #[test]
    fn secret_hash_isolates_and_is_stable() {
        let grants = parse_header(
            r#"[{"name":"tok","value":"v","entropy":"E1","readers":[{"base":"aa"}]}]"#,
        );
        let job = map(&[("base", "aa"), ("salt", "z")]);
        let h = secret_hash(&grants, &job).expect("a matching grant hashes");
        // Stable across calls (a real cache key).
        assert_eq!(Some(h.clone()), secret_hash(&grants, &job));
        // No matching grant → None, so a secret-free run stays globally shared.
        assert!(secret_hash(&grants, &map(&[("base", "zz")])).is_none());
        // Rotating the entropy re-namespaces the cache; rotating only the value
        // (same entropy) does not.
        let rotated_entropy = parse_header(
            r#"[{"name":"tok","value":"v","entropy":"E2","readers":[{"base":"aa"}]}]"#,
        );
        assert_ne!(Some(h.clone()), secret_hash(&rotated_entropy, &job));
        let rotated_value = parse_header(
            r#"[{"name":"tok","value":"DIFFERENT","entropy":"E1","readers":[{"base":"aa"}]}]"#,
        );
        assert_eq!(Some(h), secret_hash(&rotated_value, &job));
    }
}
