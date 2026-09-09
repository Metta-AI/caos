use serde_json::Value;

use super::paths::MAX_JSON_INT;

pub fn canonical_bytes(value: &Value) -> Result<Vec<u8>, String> {
    if !value.is_object() {
        return Err("canonical JSON must be an object".to_string());
    }
    let mut bytes = Vec::new();
    encode(value, &mut bytes)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn parse_canonical(bytes: &[u8]) -> Result<Value, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if canonical_bytes(&value)? != bytes {
        return Err("non-canonical JSON".to_string());
    }
    Ok(value)
}

pub fn canonical_object(pairs: &[(&str, Value)]) -> Value {
    let mut object = serde_json::Map::new();
    for (key, value) in pairs {
        object.insert((*key).to_string(), value.clone());
    }
    Value::Object(object)
}

fn encode(value: &Value, output: &mut Vec<u8>) -> Result<(), String> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(number) => {
            let Some(value) = number.as_u64() else {
                return Err(format!("non-integer or out-of-range number: {number}"));
            };
            if value > MAX_JSON_INT {
                return Err(format!("non-integer or out-of-range number: {number}"));
            }
            output.extend_from_slice(number.to_string().as_bytes());
        }
        Value::String(value) => {
            let encoded = serde_json::to_string(value).map_err(|error| error.to_string())?;
            output.extend_from_slice(encoded.as_bytes());
        }
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                encode(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(object) => {
            output.push(b'{');
            let mut entries: Vec<(&String, &Value)> = object.iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                let encoded = serde_json::to_string(key).map_err(|error| error.to_string())?;
                output.extend_from_slice(encoded.as_bytes());
                output.push(b':');
                encode(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_are_sorted_by_utf8_bytes() {
        let value = json!({"b": 1, "a": 2, "é": 3, "Z": 4});
        assert_eq!(
            canonical_bytes(&value).unwrap(),
            "{\"Z\":4,\"a\":2,\"b\":1,\"é\":3}\n".as_bytes()
        );
    }

    #[test]
    fn rejects_noncanonical_and_invalid_values() {
        for bytes in [
            b"{\"a\":1.5}\n".as_slice(),
            b"{\"a\":-1}\n",
            b"{\"a\":9007199254740992}\n",
            b"{\"a\":1,\"a\":2}\n",
            b"{\"b\":1,\"a\":2}\n",
            b"{ \"a\":1}\n",
            b"{\"a\":1}",
            b"{\"a\":1}\ntrailing",
            b"[]\n",
        ] {
            assert!(parse_canonical(bytes).is_err(), "accepted {bytes:?}");
        }
    }

    #[test]
    fn nested_values_round_trip() {
        let value = json!({
            "array": [null, true, false, "text", {"z": 2, "a": 1}],
            "object": {"nested": []}
        });
        let bytes = canonical_bytes(&value).unwrap();
        assert_eq!(parse_canonical(&bytes), Ok(value));
    }
}
