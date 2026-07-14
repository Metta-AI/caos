//! The progress ref: after each step commit, push
//! `refs/caos/progress/<conversation>` to the server so the growing step chain
//! is observable (and its commits reachable) before the turn completes.
//!
//! The worker image has no `git`, so this speaks just enough of the smart-HTTP
//! receive-pack protocol directly: every object the ref needs is already on
//! the server (the worker stored them via `/object` as it built them), so the
//! push is a single ref update plus the well-known *empty* packfile. The old
//! value comes from the receive-pack ref advertisement, exactly as git's own
//! push would learn it.
//!
//! Best-effort by design: progress is observability, not correctness, so a
//! failed push warns and moves on.

/// The empty packfile: header (`PACK`, version 2, zero objects) plus its
/// SHA-1 trailer — constant, since it has no contents.
const EMPTY_PACK: &[u8] = &[
    b'P', b'A', b'C', b'K', 0, 0, 0, 2, 0, 0, 0, 0, // header
    0x02, 0x9d, 0x08, 0x82, 0x3b, 0xd8, 0xa8, 0xea, 0xb5, 0x10, // sha1…
    0xad, 0x6a, 0xc7, 0x5c, 0x82, 0x3c, 0xfd, 0x3e, 0xd3, 0x1e,
];

const ZERO_HASH: &str = "0000000000000000000000000000000000000000";

/// Point `refs/caos/progress/<conversation>` at `new_hash`, warning (never
/// failing) on any error.
pub fn push(conversation: &str, new_hash: &str) {
    if let Err(e) = try_push(conversation, new_hash) {
        eprintln!("llm-step: progress push for {conversation:?} failed (non-fatal): {e}");
    }
}

fn try_push(conversation: &str, new_hash: &str) -> Result<(), String> {
    let base =
        std::env::var("CAOS_SERVER_URL").map_err(|_| "CAOS_SERVER_URL not set".to_string())?;
    let base = base.trim_end_matches('/');
    let refname = format!("refs/caos/progress/{conversation}");

    // Learn the ref's current value from the receive-pack advertisement — the
    // update must name it as `old` or the server rejects the push.
    let old = advertised(base, &refname)?.unwrap_or_else(|| ZERO_HASH.to_string());

    // One command pkt-line (with the capability list after NUL), flush, then
    // the empty pack — the objects are already server-side.
    let command = format!("{old} {new_hash} {refname}\0report-status");
    let mut body = pkt_line(&command);
    body.extend_from_slice(b"0000");
    body.extend_from_slice(EMPTY_PACK);

    let url = format!("{base}/git-receive-pack");
    let resp = minreq::post(&url)
        .with_header("content-type", "application/x-git-receive-pack-request")
        .with_timeout(30)
        .with_body(body)
        .send()
        .map_err(|e| format!("POST {url}: {e}"))?;
    if !(200..300).contains(&resp.status_code) {
        return Err(format!(
            "POST {url}: {} {}",
            resp.status_code, resp.reason_phrase
        ));
    }
    let report = String::from_utf8_lossy(resp.as_bytes());
    if !report.contains("unpack ok") || !report.contains(&format!("ok {refname}")) {
        return Err(format!("push not acknowledged: {}", report.trim()));
    }
    Ok(())
}

/// The hash the receive-pack advertisement currently records for `refname`,
/// or `None` if the ref doesn't exist yet.
fn advertised(base: &str, refname: &str) -> Result<Option<String>, String> {
    let url = format!("{base}/info/refs?service=git-receive-pack");
    let resp = minreq::get(&url)
        .with_timeout(30)
        .send()
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !(200..300).contains(&resp.status_code) {
        return Err(format!(
            "GET {url}: {} {}",
            resp.status_code, resp.reason_phrase
        ));
    }
    // Each advertised ref is a pkt-line `<4-hex len><40-hex hash> <refname>`,
    // the first with a NUL + capability list appended. Splitting on newlines
    // and NULs and matching on the ` <refname>` suffix sidesteps full pkt
    // parsing; the 40-hex hash sits immediately before the separating space.
    let text = String::from_utf8_lossy(resp.as_bytes()).into_owned();
    let suffix = format!(" {refname}");
    for line in text.split(['\n', '\0']) {
        if let Some(prefix) = line.strip_suffix(&suffix) {
            if prefix.len() >= 40 {
                let hash = &prefix[prefix.len() - 40..];
                if hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Ok(Some(hash.to_string()));
                }
            }
        }
    }
    Ok(None)
}

/// Encode one pkt-line: 4 hex digits of total length (including the header),
/// then the payload.
fn pkt_line(payload: &str) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(payload.as_bytes());
    out
}
