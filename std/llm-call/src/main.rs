//! A stateless, tool-free LLM call for small auxiliary jobs.
//!
//! The API key is injected as the `anthropic-api-key` secret. Optional curried
//! configuration: `base-url`. Call inputs:
//! `system`, a JSON `messages` array, and optionally `model` and `max-tokens`.
//! The result is the response's text blocks as a plain blob. This worker owns
//! no conversation commits, tools, status refs, or presentation-specific
//! prompts.

use std::fs;

use llm_client::{post_messages, DEFAULT_BASE_URL, DEFAULT_MODEL};
use serde_json::{json, Value};
use worker_common::{caos, path, read_arg, read_arg_opt, run_worker, scratch, secret};

fn main() -> std::process::ExitCode {
    run_worker("llm-call", run)
}

fn run() -> Result<(), String> {
    let api_key = secret("anthropic-api-key")?;
    let base_url = read_arg_opt("base-url")?.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let model = read_arg_opt("model")?.unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let system = read_arg("system")?;
    let messages = parse_messages(&read_arg("messages")?)?;
    let max_tokens = read_arg_opt("max-tokens")?
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| format!("max-tokens must be a positive integer, got {value:?}"))
        })
        .transpose()?
        .unwrap_or(1024);
    if max_tokens == 0 {
        return Err("max-tokens must be a positive integer".to_string());
    }

    let body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": messages,
    });
    let response = post_messages(&base_url, &api_key, &body, &|_| {})?;
    let blocks = response["content"]
        .as_array()
        .ok_or("API response has no content array")?;
    let text = response_text(blocks);
    if text.trim().is_empty() {
        return Err("API response has no text content".to_string());
    }
    let out = scratch("llm-call")?.join("response");
    fs::write(&out, text).map_err(|error| format!("writing response: {error}"))?;
    caos(["put", path(&out), "/cas/out"])
}

fn parse_messages(text: &str) -> Result<Vec<Value>, String> {
    let messages: Vec<Value> =
        serde_json::from_str(text).map_err(|error| format!("messages is invalid JSON: {error}"))?;
    if messages.is_empty() {
        return Err("messages must contain at least one message".to_string());
    }
    if messages.iter().any(|message| {
        !matches!(message["role"].as_str(), Some("user" | "assistant"))
            || !message
                .get("content")
                .is_some_and(|content| content.is_string() || content.is_array())
    }) {
        return Err("each message needs a user or assistant role and content".to_string());
    }
    Ok(messages)
}

fn response_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_require_roles_and_content() {
        assert_eq!(
            parse_messages(r#"[{"role":"user","content":"Name this"}]"#).unwrap(),
            vec![json!({"role": "user", "content": "Name this"})]
        );
        assert!(parse_messages("[]").is_err());
        assert!(parse_messages(r#"[{"role":"tool","content":"no"}]"#).is_err());
        assert!(parse_messages(r#"[{"role":"user"}]"#).is_err());
        assert!(parse_messages(r#"[{"role":"user","content":null}]"#).is_err());
    }

    #[test]
    fn response_text_ignores_non_text_blocks() {
        let blocks = vec![
            json!({"type": "thinking", "thinking": "hidden"}),
            json!({"type": "text", "text": "First"}),
            json!({"type": "text", "text": "Second"}),
        ];
        assert_eq!(response_text(&blocks), "First\nSecond");
    }
}
