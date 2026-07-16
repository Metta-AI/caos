//! The raw LLM API call: one blocking `POST /v1/messages` per step, no SDK,
//! no streaming (progress granularity is the step commit). Hand-rolled retry
//! on 429/5xx honoring `retry-after`; transport errors retry the same way.

use serde_json::Value;

/// Attempts before giving up (the run then errors and the turn fails).
const MAX_ATTEMPTS: u32 = 4;

/// Generous per-request timeout: a long adaptive-thinking round is slow, and
/// the step is the unit of progress — there is nothing finer to time out to.
const TIMEOUT_SECS: u64 = 600;

/// POST `body` to `{base_url}/v1/messages` and return the parsed response.
/// `status` receives one line per notable wait (a retry and its delay) — the
/// caller forwards it to the conversation's status ref so a slow round is
/// observable, not silent.
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
            Ok(resp) => resp.status_code == 429 || resp.status_code >= 500,
            Err(_) => true, // connection-level failure
        };
        match sent {
            Ok(resp) if (200..300).contains(&resp.status_code) => {
                let text = resp
                    .as_str()
                    .map_err(|e| format!("POST {url}: response not UTF-8: {e}"))?;
                return serde_json::from_str(text)
                    .map_err(|e| format!("POST {url}: invalid JSON response: {e}"));
            }
            _ if retriable && attempt < MAX_ATTEMPTS => {
                // Honor retry-after when the server sent one; else back off
                // exponentially. Capped so a hostile header can't park us.
                let wait = match &sent {
                    Ok(resp) => resp
                        .headers
                        .get("retry-after")
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .unwrap_or(1 << attempt),
                    Err(_) => 1 << attempt,
                }
                .min(60);
                let why = match &sent {
                    Ok(resp) => format!("{} {}", resp.status_code, resp.reason_phrase),
                    Err(e) => e.to_string(),
                };
                let line =
                    format!("{why} — retrying in {wait}s (attempt {attempt}/{MAX_ATTEMPTS})");
                eprintln!("llm-step: POST {url}: {line}");
                status(&line);
                std::thread::sleep(std::time::Duration::from_secs(wait));
            }
            Ok(resp) => {
                return Err(format!(
                    "POST {url}: {} {}: {}",
                    resp.status_code,
                    resp.reason_phrase,
                    resp.as_str().unwrap_or("").trim()
                ));
            }
            Err(e) => return Err(format!("POST {url}: {e}")),
        }
    }
}
