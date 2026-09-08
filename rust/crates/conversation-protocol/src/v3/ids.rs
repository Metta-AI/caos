use serde_json::Value;
use sha2::{Digest, Sha256};

use super::canonical::{canonical_bytes, canonical_object};
use super::oid::{hex_lower, is_lower_hex, Oid};

pub fn protocol_id(tag: &str, fields: &Value) -> Result<String, String> {
    if tag.is_empty() || !tag.is_ascii() || tag.as_bytes().contains(&0) {
        return Err(format!("invalid protocol id tag {tag:?}"));
    }
    let mut hasher = Sha256::new();
    hasher.update(b"caos-v3-id\0");
    hasher.update(tag.as_bytes());
    hasher.update([0]);
    hasher.update(canonical_bytes(fields)?);
    Ok(hex_lower(&hasher.finalize()))
}

pub fn child_id(parent: &str, request: &Oid, round: u64, tool: &str) -> Result<String, String> {
    let fields = canonical_object(&[
        ("parent", Value::String(parent.to_string())),
        ("request", Value::String(request.as_str().to_string())),
        ("round", Value::from(round)),
        ("tool", Value::String(tool.to_string())),
    ]);
    Ok(format!("subagent-{}", protocol_id("subagent", &fields)?))
}

pub fn projection_id(descriptor: &Value) -> Result<String, String> {
    protocol_id("projection", descriptor)
}

pub fn publication_id(
    conversation: &str,
    key: &str,
    qp: &str,
    planned_head: &Oid,
    repository: &str,
    refname: &str,
    expected_old: Option<&Oid>,
) -> Result<String, String> {
    let fields = canonical_object(&[
        ("conversation", Value::String(conversation.to_string())),
        ("key", Value::String(key.to_string())),
        ("qp", Value::String(qp.to_string())),
        (
            "planned_head",
            Value::String(planned_head.as_str().to_string()),
        ),
        ("repository", Value::String(repository.to_string())),
        ("ref", Value::String(refname.to_string())),
        (
            "expected_old",
            expected_old
                .map(|oid| Value::String(oid.as_str().to_string()))
                .unwrap_or(Value::Null),
        ),
    ]);
    protocol_id("publication", &fields)
}

pub fn validate_client_key(key: &str) -> Result<(), String> {
    if key.len() != 32 || !is_lower_hex(key) {
        return Err(format!(
            "client key must be 32 lowercase hexadecimal characters, got {key:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn protocol_id_known_answers() {
        assert_eq!(
            protocol_id("t", &json!({"a": 1})).unwrap(),
            "589086ca616938bbd6e0f55090b3d91d3945e9c43180b032b2dd534b45d63eb5"
        );
        let request = Oid::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "request").unwrap();
        assert_eq!(
            child_id("talk-1", &request, 3, "toolu_01AB").unwrap(),
            "subagent-a9278ef7d75f32ccaf0e1a1c0aa64909fb37efbf01bd9d95eda62a8c2f4a1ea4"
        );
        assert_eq!(
            projection_id(&json!({
                "source_base": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "source_head": "cccccccccccccccccccccccccccccccccccccccc",
                "target_base": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "policy": "squash",
                "implementation": "caos-project-v1",
                "commit_policy": "single"
            }))
            .unwrap(),
            "012fc74e5001a228f9a634ef5d9c8c9d14a3c7e0e1f7090371ab70d66f23d7a4"
        );
    }

    #[test]
    fn client_keys_are_canonical() {
        assert!(validate_client_key("0123456789abcdef0123456789abcdef").is_ok());
        for key in [
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdeg",
        ] {
            assert!(validate_client_key(key).is_err(), "accepted {key:?}");
        }
    }
}
