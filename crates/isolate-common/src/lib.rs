//! Guest-side helpers for isolate workers.
//!
//! The only host surface is the `caos_abi_v1` JSON `call`/`read` pair. This
//! crate keeps wasm workers out of ABI pointer plumbing and gives them typed
//! wrappers for CAS objects, nested runs, and final output.

use serde_json::{json, Value};

#[cfg(target_arch = "wasm32")]
mod raw {
    #[link(wasm_import_module = "caos_abi_v1")]
    extern "C" {
        fn call(ptr: i32, len: i32) -> i32;
        fn read(ptr: i32);
    }

    pub unsafe fn host_call(ptr: *const u8, len: usize) -> i32 {
        call(ptr as i32, len as i32)
    }

    pub unsafe fn host_read(ptr: *mut u8) {
        read(ptr as i32);
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod raw {
    pub unsafe fn host_call(_ptr: *const u8, _len: usize) -> i32 {
        panic!("caos_abi_v1::call is only available on wasm32");
    }

    pub unsafe fn host_read(_ptr: *mut u8) {
        panic!("caos_abi_v1::read is only available on wasm32");
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    pub args: String,
    pub std: String,
    pub salt: String,
    pub stack: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub name: String,
    pub kind: String,
    pub hash: String,
    pub mode: Option<String>,
}

impl Entry {
    pub fn blob(name: impl Into<String>, hash: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: "blob".to_string(),
            hash: hash.into(),
            mode: None,
        }
    }

    pub fn tree(name: impl Into<String>, hash: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: "tree".to_string(),
            hash: hash.into(),
            mode: None,
        }
    }

    pub fn is_tree(&self) -> bool {
        self.kind == "tree" || matches!(self.mode.as_deref(), Some("40000") | Some("040000"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    pub image: String,
    pub args: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunResult {
    pub kind: String,
    pub hash: String,
}

pub fn op(req: &Value) -> Result<Value, String> {
    let bytes = serde_json::to_vec(req).map_err(|e| format!("encoding op: {e}"))?;
    if bytes.len() > i32::MAX as usize {
        return Err("op request too large".to_string());
    }
    let len = unsafe { raw::host_call(bytes.as_ptr(), bytes.len()) };
    if len < 0 {
        return Err(format!("host returned negative response length {len}"));
    }
    let mut response = vec![0_u8; len as usize];
    unsafe { raw::host_read(response.as_mut_ptr()) };
    let value: Value =
        serde_json::from_slice(&response).map_err(|e| format!("decoding response: {e}"))?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(error.to_string());
    }
    Ok(value)
}

pub fn job() -> Result<Job, String> {
    let value = op(&json!({ "op": "job" }))?;
    Ok(Job {
        args: string_field(&value, "args")?.to_string(),
        std: string_field(&value, "std")?.to_string(),
        salt: string_field(&value, "salt")?.to_string(),
        stack: string_field(&value, "stack")?.to_string(),
    })
}

pub fn tree(hash: &str) -> Result<Vec<Entry>, String> {
    let value = op(&json!({ "op": "tree", "hash": hash }))?;
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "tree response missing entries".to_string())?;
    let mut parsed = Vec::with_capacity(entries.len());
    for entry in entries {
        parsed.push(Entry {
            name: string_field(entry, "name")?.to_string(),
            kind: string_field(entry, "kind")?.to_string(),
            hash: string_field(entry, "hash")?.to_string(),
            mode: string_field_opt(entry, "mode")?,
        });
    }
    Ok(parsed)
}

pub fn get(hash: &str) -> Result<Vec<u8>, String> {
    let value = op(&json!({ "op": "get", "hash": hash }))?;
    base64_decode(string_field(&value, "bytes_b64")?)
}

pub fn put_blob(bytes: &[u8]) -> Result<String, String> {
    let value = op(&json!({ "op": "put_blob", "bytes_b64": base64_encode(bytes) }))?;
    Ok(string_field(&value, "hash")?.to_string())
}

pub fn put_tree(entries: &[Entry]) -> Result<String, String> {
    let json_entries = entries.iter().map(entry_json).collect::<Vec<Value>>();
    let value = op(&json!({ "op": "put_tree", "entries": json_entries }))?;
    Ok(string_field(&value, "hash")?.to_string())
}

pub fn run(image: &str, args_hash: &str) -> Result<RunResult, String> {
    let value = op(&json!({ "op": "run", "image": image, "args": args_hash }))?;
    run_result(&value)
}

pub fn run_many(reqs: &[RunRequest]) -> Result<Vec<Result<RunResult, String>>, String> {
    let json_reqs = reqs
        .iter()
        .map(|req| json!({ "image": req.image, "args": req.args }))
        .collect::<Vec<Value>>();
    let value = op(&json!({ "op": "run_many", "reqs": json_reqs }))?;
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| "run_many response missing results".to_string())?;
    let mut parsed = Vec::with_capacity(results.len());
    for result in results {
        if let Some(error) = result.get("error").and_then(Value::as_str) {
            parsed.push(Err(error.to_string()));
        } else {
            parsed.push(run_result(result));
        }
    }
    Ok(parsed)
}

pub fn out(kind: &str, hash: &str) -> Result<(), String> {
    op(&json!({ "op": "out", "kind": kind, "hash": hash })).map(|_| ())
}

pub fn log(msg: &str) -> Result<(), String> {
    op(&json!({ "op": "log", "msg": msg })).map(|_| ())
}

pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let _ = crate::log(&format!("panic: {info}"));
    }));
}

#[macro_export]
macro_rules! entry {
    ($run:path) => {
        #[no_mangle]
        pub extern "C" fn caos_run() {
            $crate::install_panic_hook();
            match std::panic::catch_unwind(|| $run()) {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    let _ = $crate::log(&err);
                    panic!("caos_run failed: {err}");
                }
                Err(payload) => {
                    let _ = $crate::log("caos_run panicked");
                    std::panic::resume_unwind(payload);
                }
            }
        }
    };
}

fn entry_json(entry: &Entry) -> Value {
    let mut value = json!({
        "name": entry.name,
        "kind": entry.kind,
        "hash": entry.hash,
    });
    if let Some(mode) = &entry.mode {
        value["mode"] = Value::String(mode.clone());
    }
    value
}

fn run_result(value: &Value) -> Result<RunResult, String> {
    Ok(RunResult {
        kind: string_field(value, "kind")?.to_string(),
        hash: string_field(value, "hash")?.to_string(),
    })
}

fn string_field<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string field {key:?}"))
}

fn string_field_opt(value: &Value, key: &str) -> Result<Option<String>, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("field {key:?} must be a string")),
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err("base64 length is not a multiple of 4".to_string());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut vals = [0_u8; 4];
        let mut pad = 0;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte == b'=' {
                pad += 1;
                vals[index] = 0;
            } else {
                vals[index] = base64_value(*byte)?;
            }
        }
        if pad > 2 || (pad > 0 && chunk[3] != b'=') || (pad == 2 && chunk[2] != b'=') {
            return Err("invalid base64 padding".to_string());
        }
        let n = ((vals[0] as u32) << 18)
            | ((vals[1] as u32) << 12)
            | ((vals[2] as u32) << 6)
            | vals[3] as u32;
        out.push(((n >> 16) & 0xff) as u8);
        if pad < 2 {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if pad < 1 {
            out.push((n & 0xff) as u8);
        }
    }
    Ok(out)
}

fn base64_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(format!("invalid base64 byte 0x{byte:02x}")),
    }
}
