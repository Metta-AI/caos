//! Shared transport for Anthropic Messages API calls made by CAOS workers.
//!
//! This crate deliberately owns only provider transport, defaults, and retry
//! behavior. Conversation commits, tools, progress refs, and response policy
//! remain responsibilities of the worker making the call.

use serde_json::Value;

pub const DEFAULT_MODEL: &str = "claude-opus-4-8";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Attempts before giving up and failing the worker invocation.
const MAX_ATTEMPTS: u32 = 4;

/// A long adaptive-thinking round may legitimately take several minutes.
const TIMEOUT_SECS: u64 = 600;

/// POST `body` to `{base_url}/v1/messages` and return the parsed response.
///
/// `status` receives one line per retry so a caller with a durable progress
/// surface can expose the wait. Stateless callers may pass a no-op closure.
pub fn post_messages(
    base_url: &str,
    api_key: &str,
    body: &Value,
    status: &dyn Fn(&str),
) -> Result<Value, String> {
    let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
    let payload = body.to_string();
    let mut attempt = 0;
    loop {
        attempt += 1;
        let sent = minreq::post(&url)
            .with_header("x-api-key", api_key)
            .with_header("anthropic-version", "2023-06-01")
            .with_header("content-type", "application/json")
            .with_timeout(TIMEOUT_SECS)
            .with_body(payload.clone())
            .send();
        let retriable = match &sent {
            Ok(response) => response.status_code == 429 || response.status_code >= 500,
            Err(_) => true,
        };
        match sent {
            Ok(response) if (200..300).contains(&response.status_code) => {
                let text = response
                    .as_str()
                    .map_err(|error| format!("POST {url}: response not UTF-8: {error}"))?;
                return serde_json::from_str(text)
                    .map_err(|error| format!("POST {url}: invalid JSON response: {error}"));
            }
            _ if retriable && attempt < MAX_ATTEMPTS => {
                let wait = match &sent {
                    Ok(response) => response
                        .headers
                        .get("retry-after")
                        .and_then(|value| value.trim().parse::<u64>().ok())
                        .unwrap_or(1 << attempt),
                    Err(_) => 1 << attempt,
                }
                .min(60);
                let why = match &sent {
                    Ok(response) => {
                        format!("{} {}", response.status_code, response.reason_phrase)
                    }
                    Err(error) => error.to_string(),
                };
                let line =
                    format!("{why} — retrying in {wait}s (attempt {attempt}/{MAX_ATTEMPTS})");
                eprintln!("llm: POST {url}: {line}");
                status(&line);
                std::thread::sleep(std::time::Duration::from_secs(wait));
            }
            Ok(response) => {
                return Err(format!(
                    "POST {url}: {} {}: {}",
                    response.status_code,
                    response.reason_phrase,
                    response.as_str().unwrap_or("").trim()
                ));
            }
            Err(error) => return Err(format!("POST {url}: {error}")),
        }
    }
}
