//! The `github` tool: one GitHub API call, made inline by the step
//! (design/agent-github.md, "PRs"). A PR's commits go with `publish_source`;
//! everything else about it is metadata, and this sends it. Nothing is checked
//! out, and no worker runs: a worker's result would be memoized by its ArgTree,
//! while this answer depends on GitHub's state and may change it.
//!
//! A GET runs, and a retry runs it again. Any other method is a write, pinned
//! in a `tool.start` before it is sent; only the attempt that appended the pin
//! sends it, and an attempt that finds the call started and unfinished
//! completes it as not confirmed rather than sending it a second time.
use super::*;

pub(super) const NAME: &str = "github";

pub(super) const HELP: &str = "Call the GitHub API (REST, or GraphQL as POST /graphql) at api.github.com with the granted github-token, without checking anything out. Use it for everything about a PR except its commits, which publish_source pushes: find a branch's open PR, open one, change its base, link PRs as a stack. Returns the HTTP status and the response body; a status outside 2xx is an error. A GET only reads, and is safe to retry. Any other method is a write and is sent at most once: if an earlier attempt sent it without recording the response, the result says it is not confirmed instead of sending it again. Then read the affected state with a GET before deciding whether to retry. Long responses are truncated; narrow the query, or select fields with GraphQL.
@param method GET, POST, PATCH, PUT or DELETE.
@param path API path with any query string, such as /repos/owner/repo/pulls?head=owner:branch&state=open.
@param [body] JSON request body, such as {\"title\":\"Add the parser\",\"head\":\"parser\",\"base\":\"main\",\"body\":\"Why.\"} for POST /repos/owner/repo/pulls.";

/// The one host this tool reaches. A path is appended to it, never parsed into
/// a URL of its own, and redirects are not followed, so the token is sent here
/// and nowhere else.
const API: &str = "https://api.github.com";
const TOKEN: &str = "/secret/github-token";
const TIMEOUT_SECS: u64 = 60;
/// Responses longer than this are cut, with a note saying where.
const MAX_BODY_BYTES: usize = 100_000;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    method: String,
    path: String,
    #[serde(default)]
    body: Option<Value>,
}

/// What is sent, exactly. The body is kept as the text that goes on the wire:
/// that is what the pin records, and canonical JSON could not hold every body
/// a model writes (it has no negative or fractional numbers).
#[derive(Debug, PartialEq, serde::Serialize)]
struct Request {
    method: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
}

fn request(call: &Call) -> Result<Request, String> {
    let p: Parameters = serde_json::from_value(call.input.clone())
        .map_err(|e| format!("invalid github arguments: {e}"))?;
    let method = p.method.trim().to_ascii_uppercase();
    if !matches!(method.as_str(), "GET" | "POST" | "PATCH" | "PUT" | "DELETE") {
        return Err(format!(
            "method must be GET, POST, PATCH, PUT or DELETE, not {:?}",
            p.method
        ));
    }
    // A model often copies a full `url` out of an earlier response.
    let path = p.path.trim();
    let path = path.strip_prefix(API).unwrap_or(path);
    if !path.starts_with('/') || path.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "path must be an API path such as /repos/owner/repo/pulls, not {:?}",
            p.path
        ));
    }
    if method == "GET" && p.body.is_some() {
        return Err("a GET takes no body; put its parameters in the query string".into());
    }
    Ok(Request {
        method,
        path: path.to_string(),
        body: p.body.map(|body| body.to_string()),
    })
}

pub(super) fn execute(state: &mut progress::State, site: &CallSite<'_>) -> Result<(), String> {
    let token = match fs::read_to_string(TOKEN) {
        Ok(token) => Some(token.trim_end().to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err("reading GitHub token".into()),
    };
    // A line break inside a header value would end the header block early.
    if token.as_deref().is_some_and(|t| t.contains(['\r', '\n'])) {
        return Err("the github-token secret has a line break inside it".into());
    }
    execute_at(state, site, API, token.as_deref())
}

pub(super) fn execute_at<S: progress::RefStore>(
    state: &mut progress::State<S>,
    site: &CallSite<'_>,
    api: &str,
    token: Option<&str>,
) -> Result<(), String> {
    let request = match request(site.call) {
        Ok(request) => request,
        Err(error) => return finish(state, site, &error, true),
    };
    if request.method == "GET" {
        let (text, is_error) = match send(api, token, &request) {
            Ok(response) => render(&response, token.is_some()),
            Err(error) => (format!("GET {} failed: {error}", request.path), true),
        };
        return finish(state, site, &text, is_error);
    }
    match pin(state, site, &request)? {
        Pin::Done => Ok(()),
        Pin::Unconfirmed => finish(
            state,
            site,
            &unconfirmed(
                &request,
                "another attempt started it and recorded no response",
            ),
            true,
        ),
        Pin::Ours => {
            let (text, is_error) = match send(api, token, &request) {
                Ok(response) => render(&response, token.is_some()),
                Err(error) => (
                    unconfirmed(&request, &format!("it failed in transit ({error})")),
                    true,
                ),
            };
            finish(state, site, &text, is_error)
        }
    }
}

fn unconfirmed(request: &Request, why: &str) -> String {
    format!(
        "Not confirmed: {} {}: {why}, so GitHub may or may not have applied it. Read the affected state with a GET before retrying.",
        request.method, request.path
    )
}

enum Pin {
    /// This attempt appended the pin, and only this attempt sends the request.
    Ours,
    /// Another attempt pinned it and has not recorded a response.
    Unconfirmed,
    /// The call already has its result.
    Done,
}

/// Record the write as started before it is sent.
///
/// The payload carries a nonce for this attempt. Two attempts pinning the same
/// request would otherwise mint the same commit, and each would see its own
/// append land and send.
fn pin<S: progress::RefStore>(
    state: &mut progress::State<S>,
    site: &CallSite<'_>,
    request: &Request,
) -> Result<Pin, String> {
    let attempt = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "clock precedes Unix epoch")?
            .as_nanos()
    );
    let payload = canonical_payload_bytes(&json!({"attempt": attempt, "request": request}))?;
    for _ in 0..32 {
        state.reload()?;
        if let Some(record) = state
            .conversation()?
            .tool(site.request, site.round, &site.call.id)?
        {
            return Ok(if record.is_terminal() {
                Pin::Done
            } else {
                Pin::Unconfirmed
            });
        }
        let mut record = site.stub(None);
        record.status = CallStatus::Started;
        let expected = state.head().clone();
        if let progress::TryAppend::Appended(_) = state.try_append_at(
            &expected,
            Transition::ToolStart {
                record,
                payloads: vec![("github.json".into(), payload.clone())],
            },
        )? {
            return Ok(Pin::Ours);
        }
    }
    Err("conversation kept moving while pinning a GitHub write".into())
}

/// Record the call's result, unless an attempt already has.
fn finish<S: progress::RefStore>(
    state: &mut progress::State<S>,
    site: &CallSite<'_>,
    text: &str,
    is_error: bool,
) -> Result<(), String> {
    let block = result_block(&site.call.id, text, is_error);
    for _ in 0..32 {
        state.reload()?;
        if state
            .conversation()?
            .tool(site.request, site.round, &site.call.id)?
            .is_some_and(|r| r.is_terminal())
        {
            return Ok(());
        }
        let stub = site.stub(None);
        let record = completed_record(
            &stub,
            ToolResult::Complete {
                observation: observation_path(&stub),
                proposal: None,
            },
            None,
        );
        let expected = state.head().clone();
        if let progress::TryAppend::Appended(_) = state.try_append_at(
            &expected,
            tool_complete_transition(record, &block, Vec::new())?,
        )? {
            return Ok(());
        }
    }
    Err("conversation kept moving while recording a GitHub call".into())
}

struct Response {
    status: i32,
    reason: String,
    location: Option<String>,
    body: String,
}

fn send(api: &str, token: Option<&str>, request: &Request) -> Result<Response, String> {
    let method = match request.method.as_str() {
        "GET" => minreq::Method::Get,
        "POST" => minreq::Method::Post,
        "PATCH" => minreq::Method::Patch,
        "PUT" => minreq::Method::Put,
        _ => minreq::Method::Delete,
    };
    let mut http = minreq::Request::new(method, format!("{api}{}", request.path))
        .with_header("accept", "application/vnd.github+json")
        .with_header("x-github-api-version", "2022-11-28")
        .with_header("user-agent", "caos-llm-step")
        .with_timeout(TIMEOUT_SECS)
        // A redirect would carry the token to wherever it points.
        .with_follow_redirects(false);
    if let Some(token) = token {
        http = http.with_header("authorization", format!("Bearer {token}"));
    }
    if let Some(body) = &request.body {
        http = http
            .with_header("content-type", "application/json")
            .with_body(body.as_str());
    }
    let response = http.send().map_err(|error| error.to_string())?;
    Ok(Response {
        status: response.status_code,
        reason: response.reason_phrase.clone(),
        location: response.headers.get("location").cloned(),
        body: String::from_utf8_lossy(response.as_bytes()).into_owned(),
    })
}

/// The status line, then the body, cut if long. A status outside 2xx is an
/// error the model reacts to, never a worker failure.
fn render(response: &Response, has_token: bool) -> (String, bool) {
    let mut text = format!("{} {}", response.status, response.reason);
    if let Some(location) = &response.location {
        text.push_str(&format!("\nlocation: {location}"));
    }
    if response.status == 401 && !has_token {
        text.push_str(
            "\nNo github-token secret is granted to this step, so only public reads work.",
        );
    }
    text.push_str("\n\n");
    let body = &response.body;
    if body.len() <= MAX_BODY_BYTES {
        text.push_str(body);
    } else {
        let mut cut = MAX_BODY_BYTES;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        text.push_str(&body[..cut]);
        text.push_str(&format!(
            "\n[truncated: the first {cut} of {} bytes. Narrow the query, or select fields with GraphQL.]",
            body.len()
        ));
    }
    (text, !(200..300).contains(&response.status))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(input: Value) -> Call {
        Call {
            id: "a".into(),
            name: NAME.into(),
            input,
        }
    }

    #[test]
    fn requests_are_checked_before_anything_is_sent() {
        let parsed = request(&call(json!({
            "method": "post",
            "path": "https://api.github.com/repos/o/r/pulls",
            "body": {"head": "feature", "base": "main", "title": "T"}
        })))
        .unwrap();
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.path, "/repos/o/r/pulls");
        assert_eq!(
            serde_json::from_str::<Value>(parsed.body.as_deref().unwrap()).unwrap(),
            json!({"head": "feature", "base": "main", "title": "T"})
        );
        // A body that canonical JSON cannot hold is still sent as written.
        assert!(request(&call(json!({
            "method": "PATCH", "path": "/x", "body": {"line": -1, "ratio": 0.5}
        })))
        .is_ok());
        for input in [
            json!({}),
            json!({"method": "GET"}),
            json!({"method": "FETCH", "path": "/user"}),
            json!({"method": "GET", "path": "user"}),
            json!({"method": "GET", "path": "https://evil.example/user"}),
            json!({"method": "GET", "path": "/user name"}),
            json!({"method": "GET", "path": "/user\r\nx-injected: 1"}),
            json!({"method": "GET", "path": "/user", "body": {}}),
            json!({"method": "GET", "path": "/user", "extra": 1}),
        ] {
            assert!(request(&call(input.clone())).is_err(), "{input}");
        }
    }

    fn response(status: i32, body: &str) -> Response {
        Response {
            status,
            reason: "Reason".into(),
            location: None,
            body: body.into(),
        }
    }

    #[test]
    fn a_response_is_its_status_then_its_body_cut_at_a_char_boundary() {
        assert_eq!(
            render(&response(201, "{}"), true),
            ("201 Reason\n\n{}".to_string(), false)
        );
        let (text, is_error) = render(&response(401, "{}"), false);
        assert!(is_error);
        assert!(text.contains("No github-token secret is granted"), "{text}");
        assert!(!render(&response(401, "{}"), true)
            .0
            .contains("No github-token"));
        let long = format!("x{}", "é".repeat(MAX_BODY_BYTES));
        let (text, _) = render(&response(200, &long), true);
        assert!(text.ends_with(&format!(
            "[truncated: the first {} of {} bytes. Narrow the query, or select fields with GraphQL.]",
            MAX_BODY_BYTES - 1,
            long.len()
        )));
    }
}
