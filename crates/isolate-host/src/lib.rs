//! isolate-host: embeds wasmtime to run isolate-class workers.
//!
//! An isolate worker is a `wasm32-wasip1` module with two host imports under
//! `caos_abi_v1`: JSON `call`/`read` for storage and nested compute. The host
//! supplies deterministic WASI stubs only because Rust's WASI std links them.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gix::objs::WriteTo;
use serde_json::{json, Value};
use tiny_http::{Method, Request, Response, Server};
use wasmtime::{
    Caller, Config, Engine, InstanceAllocationStrategy, Linker, Memory, Module,
    PoolingAllocationConfig, Store,
};

const ABI_MODULE: &str = "caos_abi_v1";
const WASI_MODULE: &str = "wasi_snapshot_preview1";
const CURRY_MARKER: &str = ".caos-curry";
const MAX_RUN_MANY: usize = 16;

const ERRNO_SUCCESS: i32 = 0;
const ERRNO_BADF: i32 = 8;
const ERRNO_INVAL: i32 = 28;

/// Build the engine every isolate shares: pooling allocator so instantiation
/// is a slot reuse, not an mmap dance.
pub fn engine() -> Result<Engine, String> {
    let mut config = Config::new();
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(
        PoolingAllocationConfig::default(),
    ));
    Engine::new(&config).map_err(|e| format!("wasmtime engine: {e}"))
}

/// Risk-gate smoke test: instantiate a trivial module that round-trips through
/// one `caos_abi_v1` host import, and report how long instantiation took.
pub fn smoke() -> Result<Duration, String> {
    let wat = r#"
        (module
          (import "caos_abi_v1" "ping" (func $ping (param i32) (result i32)))
          (func (export "run") (param i32) (result i32)
            local.get 0
            call $ping))
    "#;
    let engine = engine()?;
    let module = Module::new(&engine, wat).map_err(|e| format!("compiling module: {e}"))?;
    let mut linker: Linker<()> = Linker::new(&engine);
    linker
        .func_wrap(ABI_MODULE, "ping", |x: i32| x + 1)
        .map_err(|e| format!("linking ping: {e}"))?;

    let t = Instant::now();
    let mut store = Store::new(&engine, ());
    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| format!("instantiating: {e}"))?;
    let elapsed = t.elapsed();

    let run = instance
        .get_typed_func::<i32, i32>(&mut store, "run")
        .map_err(|e| format!("looking up run: {e}"))?;
    let out = run
        .call(&mut store, 41)
        .map_err(|e| format!("calling run: {e}"))?;
    if out != 42 {
        return Err(format!("expected 42, got {out}"));
    }
    Ok(elapsed)
}

/// One tree entry as exposed over the isolate ABI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeEntry {
    pub name: String,
    pub kind: String,
    pub hash: String,
    pub mode: Option<String>,
}

/// A typed compute result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunResult {
    pub kind: String,
    pub hash: String,
}

/// Per-job parameters injected by the worker dispatcher.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    pub args: String,
    pub std: String,
    pub salt: String,
    pub stack: String,
}

/// Storage and nested-compute operations available to a guest.
pub trait Ops: Send + Sync {
    fn tree(&self, hash: &str) -> Result<Vec<TreeEntry>, String>;
    fn get(&self, hash: &str) -> Result<Vec<u8>, String>;
    fn put_blob(&self, bytes: &[u8]) -> Result<String, String>;
    fn put_tree(&self, entries: &[TreeEntry]) -> Result<String, String>;
    fn run(&self, job: &Job, image: &str, args: &str) -> Result<RunResult, String>;
}

/// HTTP implementation of [`Ops`] against a caos server.
pub struct HttpOps {
    server_url: String,
}

impl HttpOps {
    pub fn from_env() -> Result<Self, String> {
        let server_url = std::env::var("CAOS_SERVER_URL")
            .map_err(|_| "CAOS_SERVER_URL must be set".to_string())?;
        Ok(Self { server_url })
    }

    pub fn new(server_url: String) -> Self {
        Self { server_url }
    }

    fn base(&self) -> &str {
        self.server_url.trim_end_matches('/')
    }

    fn get_object(&self, hash: &str) -> Result<(String, Vec<u8>), String> {
        let url = format!("{}/object/{hash}", self.base());
        let response = minreq::get(&url)
            .send()
            .map_err(|e| format!("GET {url}: {e}"))?;
        if !(200..300).contains(&response.status_code) {
            let body = response.as_str().unwrap_or("").trim();
            let detail = if body.is_empty() {
                String::new()
            } else {
                format!(":\n{body}")
            };
            return Err(format!(
                "GET {url}: server returned {} {}{detail}",
                response.status_code, response.reason_phrase
            ));
        }
        parse_object(&response.into_bytes())
    }

    fn post_object(&self, kind: &str, content: &[u8]) -> Result<String, String> {
        let mut body = format!("{kind} {}\0", content.len()).into_bytes();
        body.extend_from_slice(content);
        let url = format!("{}/object/", self.base());
        let response = minreq::post(&url)
            .with_body(body)
            .send()
            .map_err(|e| format!("POST {url}: {e}"))?;
        if !(200..300).contains(&response.status_code) {
            let body = response.as_str().unwrap_or("").trim();
            let detail = if body.is_empty() {
                String::new()
            } else {
                format!(":\n{body}")
            };
            return Err(format!(
                "POST {url}: server returned {} {}{detail}",
                response.status_code, response.reason_phrase
            ));
        }
        let hash = response
            .as_str()
            .map_err(|e| format!("POST {url}: invalid response: {e}"))?
            .trim()
            .to_string();
        validate_hash(&hash)?;
        Ok(hash)
    }
}

impl Ops for HttpOps {
    fn tree(&self, hash: &str) -> Result<Vec<TreeEntry>, String> {
        let (kind, content) = self.get_object(hash)?;
        if kind != "tree" {
            return Err(format!("expected tree, got {kind} for {hash}"));
        }
        parse_tree_entries(hash, &content)
    }

    fn get(&self, hash: &str) -> Result<Vec<u8>, String> {
        let (kind, content) = self.get_object(hash)?;
        if kind != "blob" {
            return Err(format!("expected blob, got {kind} for {hash}"));
        }
        Ok(content)
    }

    fn put_blob(&self, bytes: &[u8]) -> Result<String, String> {
        self.post_object("blob", bytes)
    }

    fn put_tree(&self, entries: &[TreeEntry]) -> Result<String, String> {
        let bytes = encode_tree_entries(entries)?;
        self.post_object("tree", &bytes)
    }

    fn run(&self, job: &Job, image: &str, args: &str) -> Result<RunResult, String> {
        let (image, args) = resolve_curry(self, &job.std, image, args)?;
        let req = build_request(self, &image, &args, &job.std, &job.salt)?;
        let mut url = format!("{}/run?req={req}", self.base());
        if !job.stack.is_empty() {
            url.push_str("&stack=");
            url.push_str(&percent_encode(&job.stack));
        }
        let response = minreq::get(&url)
            .send()
            .map_err(|e| format!("GET {url}: {e}"))?;
        if !(200..300).contains(&response.status_code) {
            let body = response.as_str().unwrap_or("").trim();
            let detail = if body.is_empty() {
                String::new()
            } else {
                format!(":\n{body}")
            };
            return Err(format!(
                "GET {url}: server returned {} {}{detail}",
                response.status_code, response.reason_phrase
            ));
        }
        let text = response
            .as_str()
            .map_err(|e| format!("GET {url}: invalid response: {e}"))?
            .trim();
        parse_run_result(text)
    }
}

/// Result and timings for a single isolate execution.
pub struct Execution {
    pub result: RunResult,
    pub instantiate: Duration,
    pub guest: Duration,
}

struct HostState {
    ops: Arc<dyn Ops>,
    job: Job,
    response: Vec<u8>,
    out: Option<RunResult>,
    out_calls: usize,
}

/// Run one compiled module instance for one job.
pub fn run_module(module: &Module, ops: Arc<dyn Ops>, job: Job) -> Result<Execution, String> {
    let engine = module.engine();
    let mut linker: Linker<HostState> = Linker::new(engine);
    define_abi(&mut linker)?;
    define_wasi(&mut linker)?;

    let state = HostState {
        ops,
        job,
        response: Vec::new(),
        out: None,
        out_calls: 0,
    };
    let mut store = Store::new(engine, state);

    let t_instantiate = Instant::now();
    let instance = linker
        .instantiate(&mut store, module)
        .map_err(|e| format!("instantiating guest: {e}"))?;
    let instantiate = t_instantiate.elapsed();

    instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| "guest missing required export 'memory'".to_string())?;
    let run = instance
        .get_typed_func::<(), ()>(&mut store, "caos_run")
        .map_err(|e| format!("guest missing required export 'caos_run': {e}"))?;

    let t_guest = Instant::now();
    run.call(&mut store, ())
        .map_err(|e| format!("guest trap: {e}"))?;
    let guest = t_guest.elapsed();

    let state = store.data();
    if state.out_calls != 1 {
        return Err(format!(
            "guest must call out exactly once, called {} times",
            state.out_calls
        ));
    }
    let result = state
        .out
        .clone()
        .ok_or_else(|| "guest returned without an out result".to_string())?;
    Ok(Execution {
        result,
        instantiate,
        guest,
    })
}

/// Serve isolate jobs on `[::]:8080`.
pub fn serve() -> Result<(), String> {
    let ops: Arc<dyn Ops> = Arc::new(HttpOps::from_env()?);
    serve_with_ops(ops)
}

pub fn serve_with_ops(ops: Arc<dyn Ops>) -> Result<(), String> {
    let engine = Arc::new(engine()?);
    let modules: Arc<Mutex<HashMap<String, Module>>> = Arc::new(Mutex::new(HashMap::new()));
    let server = Server::http("[::]:8080").map_err(|e| format!("binding :8080: {e}"))?;
    eprintln!("isolate-host: listening on :8080");
    for request in server.incoming_requests() {
        let ops = Arc::clone(&ops);
        let engine = Arc::clone(&engine);
        let modules = Arc::clone(&modules);
        std::thread::spawn(move || handle_request(request, ops, engine, modules));
    }
    Ok(())
}

struct ServeJob {
    module: String,
    job: Job,
}

fn handle_request(
    mut request: Request,
    ops: Arc<dyn Ops>,
    engine: Arc<Engine>,
    modules: Arc<Mutex<HashMap<String, Module>>>,
) {
    if request.method() != &Method::Post || request.url().split('?').next() != Some("/run") {
        return respond(request, 404, "not found");
    }

    let total_start = Instant::now();
    let mut body = String::new();
    if request.as_reader().read_to_string(&mut body).is_err() {
        return respond(request, 400, "unreadable body");
    }
    let serve_job = match parse_serve_job(&body) {
        Ok(job) => job,
        Err(e) => return respond(request, 400, &e),
    };

    let module_key = serve_job.module.clone();
    let (module, compile_ms) = match load_module(&engine, &modules, &ops, &module_key) {
        Ok(result) => result,
        Err(e) => {
            trace_isolate(&module_key, 0, Duration::ZERO, Duration::ZERO, total_start);
            return respond(request, 500, &e);
        }
    };

    match run_module(&module, ops, serve_job.job) {
        Ok(execution) => {
            trace_isolate(
                &module_key,
                compile_ms,
                execution.instantiate,
                execution.guest,
                total_start,
            );
            respond(
                request,
                200,
                &format!("{} {}\n", execution.result.kind, execution.result.hash),
            );
        }
        Err(e) => {
            trace_isolate(
                &module_key,
                compile_ms,
                Duration::ZERO,
                Duration::ZERO,
                total_start,
            );
            respond(request, 500, &e);
        }
    }
}

fn load_module(
    engine: &Engine,
    modules: &Mutex<HashMap<String, Module>>,
    ops: &Arc<dyn Ops>,
    module_hash: &str,
) -> Result<(Module, u128), String> {
    validate_hash(module_hash)?;
    let mut locked = modules.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(module) = locked.get(module_hash) {
        return Ok((module.clone(), 0));
    }

    let wasm = ops.get(module_hash)?;
    let t_compile = Instant::now();
    let module = Module::from_binary(engine, &wasm)
        .map_err(|e| format!("compiling module {module_hash}: {e}"))?;
    let compile_ms = t_compile.elapsed().as_millis();
    locked.insert(module_hash.to_string(), module.clone());
    Ok((module, compile_ms))
}

fn trace_isolate(
    module_hash: &str,
    compile_ms: u128,
    instantiate: Duration,
    guest: Duration,
    total_start: Instant,
) {
    let module = module_hash
        .get(..module_hash.len().min(12))
        .unwrap_or(module_hash);
    eprintln!(
        "isolate-trace module={module} compile_ms={compile_ms} instantiate_us={} guest_ms={} total_ms={}",
        instantiate.as_micros(),
        guest.as_millis(),
        total_start.elapsed().as_millis()
    );
}

fn parse_serve_job(body: &str) -> Result<ServeJob, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("invalid job json: {e}"))?;
    let module = json_string(&value, "module")?;
    let args = json_string(&value, "args")?;
    validate_hash(module)?;
    validate_hash(args)?;
    let std = json_string_opt(&value, "std")?.unwrap_or_default();
    if !std.is_empty() {
        validate_hash(&std)?;
    }
    Ok(ServeJob {
        module: module.to_string(),
        job: Job {
            args: args.to_string(),
            std,
            salt: json_string_opt(&value, "salt")?.unwrap_or_default(),
            stack: json_string_opt(&value, "stack")?.unwrap_or_default(),
        },
    })
}

fn respond(request: Request, status: u16, body: &str) {
    let response = Response::from_string(body.to_string()).with_status_code(status);
    let _ = request.respond(response);
}

fn define_abi(linker: &mut Linker<HostState>) -> Result<(), String> {
    linker
        .func_wrap(ABI_MODULE, "call", host_call)
        .map_err(|e| format!("linking {ABI_MODULE}.call: {e}"))?;
    linker
        .func_wrap(ABI_MODULE, "read", host_read)
        .map_err(|e| format!("linking {ABI_MODULE}.read: {e}"))?;
    Ok(())
}

fn host_call(mut caller: Caller<'_, HostState>, ptr: i32, len: i32) -> i32 {
    let request = match read_guest(&mut caller, ptr, len) {
        Ok(bytes) => bytes,
        Err(e) => {
            return stash_response(caller.data_mut(), json!({ "error": e }));
        }
    };
    let value: Value = match serde_json::from_slice(&request) {
        Ok(value) => value,
        Err(e) => {
            return stash_response(
                caller.data_mut(),
                json!({ "error": format!("invalid json: {e}") }),
            );
        }
    };
    let response = match execute_op(caller.data_mut(), value) {
        Ok(value) => value,
        Err(e) => json!({ "error": e }),
    };
    stash_response(caller.data_mut(), response)
}

fn host_read(mut caller: Caller<'_, HostState>, ptr: i32) {
    if ptr < 0 {
        eprintln!("guest read with negative pointer {ptr}");
        return;
    }
    let memory = match guest_memory(&mut caller) {
        Ok(memory) => memory,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    let response = caller.data().response.clone();
    if let Err(e) = memory.write(&mut caller, ptr as usize, &response) {
        eprintln!("guest read failed: {e}");
    }
}

fn read_guest(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> Result<Vec<u8>, String> {
    if ptr < 0 || len < 0 {
        return Err(format!("negative guest pointer/len ptr={ptr} len={len}"));
    }
    let memory = guest_memory(caller)?;
    let mut bytes = vec![0; len as usize];
    memory
        .read(caller, ptr as usize, &mut bytes)
        .map_err(|e| format!("reading guest memory: {e}"))?;
    Ok(bytes)
}

fn guest_memory(caller: &mut Caller<'_, HostState>) -> Result<Memory, String> {
    caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| "guest missing required export 'memory'".to_string())
}

fn stash_response(state: &mut HostState, value: Value) -> i32 {
    let bytes = match serde_json::to_vec(&value) {
        Ok(bytes) => bytes,
        Err(e) => format!(r#"{{"error":"encoding response: {e}"}}"#).into_bytes(),
    };
    let len = bytes.len();
    state.response = bytes;
    match i32::try_from(len) {
        Ok(len) => len,
        Err(_) => {
            state.response = br#"{"error":"response too large"}"#.to_vec();
            state.response.len() as i32
        }
    }
}

fn execute_op(state: &mut HostState, value: Value) -> Result<Value, String> {
    let op = json_string(&value, "op")?;
    match op {
        "job" => Ok(json!({
            "args": state.job.args,
            "std": state.job.std,
            "salt": state.job.salt,
            "stack": state.job.stack,
        })),
        "tree" => {
            let hash = json_string(&value, "hash")?;
            let entries = state.ops.tree(hash)?;
            let json_entries = entries
                .iter()
                .map(tree_entry_to_json)
                .collect::<Vec<Value>>();
            Ok(json!({ "entries": json_entries }))
        }
        "get" => {
            let hash = json_string(&value, "hash")?;
            Ok(json!({ "bytes_b64": base64_encode(&state.ops.get(hash)?) }))
        }
        "put_blob" => {
            let encoded = json_string(&value, "bytes_b64")?;
            let bytes = base64_decode(encoded)?;
            Ok(json!({ "hash": state.ops.put_blob(&bytes)? }))
        }
        "put_tree" => {
            let entries = parse_json_entries(&value)?;
            Ok(json!({ "hash": state.ops.put_tree(&entries)? }))
        }
        "run" => {
            let image = json_string(&value, "image")?;
            let args = json_string(&value, "args")?;
            let result = state.ops.run(&state.job, image, args)?;
            Ok(json!({ "kind": result.kind, "hash": result.hash }))
        }
        "run_many" => run_many(state, &value),
        "out" => {
            let kind = json_string(&value, "kind")?;
            let hash = json_string(&value, "hash")?;
            validate_result_kind(kind)?;
            validate_hash(hash)?;
            state.out_calls += 1;
            if state.out_calls == 1 {
                state.out = Some(RunResult {
                    kind: kind.to_string(),
                    hash: hash.to_string(),
                });
            }
            if state.out_calls == 1 {
                Ok(json!({}))
            } else {
                Err("out called more than once".to_string())
            }
        }
        "log" => {
            let msg = json_string(&value, "msg")?;
            eprintln!("guest: {msg}");
            Ok(json!({}))
        }
        other => Err(format!("unknown op {other:?}")),
    }
}

fn run_many(state: &HostState, value: &Value) -> Result<Value, String> {
    let reqs = value
        .get("reqs")
        .and_then(Value::as_array)
        .ok_or_else(|| "run_many missing array 'reqs'".to_string())?;
    let mut parsed = Vec::with_capacity(reqs.len());
    for req in reqs {
        parsed.push((
            json_string(req, "image")?.to_string(),
            json_string(req, "args")?.to_string(),
        ));
    }

    let semaphore = Arc::new(Semaphore::new(MAX_RUN_MANY));
    let ops = Arc::clone(&state.ops);
    let job = state.job.clone();
    let results = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(parsed.len());
        for (image, args) in parsed {
            let semaphore = Arc::clone(&semaphore);
            let ops = Arc::clone(&ops);
            let job = job.clone();
            handles.push(scope.spawn(move || {
                let _permit = semaphore.acquire();
                ops.run(&job, &image, &args)
            }));
        }
        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            match handle.join() {
                Ok(result) => results.push(result),
                Err(_) => results.push(Err("run_many worker thread panicked".to_string())),
            }
        }
        results
    });

    let json_results = results
        .into_iter()
        .map(|result| match result {
            Ok(result) => json!({ "kind": result.kind, "hash": result.hash }),
            Err(error) => json!({ "error": error }),
        })
        .collect::<Vec<Value>>();
    Ok(json!({ "results": json_results }))
}

struct Semaphore {
    state: Mutex<usize>,
    available: Condvar,
}

impl Semaphore {
    fn new(permits: usize) -> Self {
        Self {
            state: Mutex::new(permits),
            available: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> Permit {
        let mut permits = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while *permits == 0 {
            permits = self
                .available
                .wait(permits)
                .unwrap_or_else(|p| p.into_inner());
        }
        *permits -= 1;
        Permit {
            semaphore: Arc::clone(self),
        }
    }
}

struct Permit {
    semaphore: Arc<Semaphore>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut permits = self
            .semaphore
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *permits += 1;
        self.semaphore.available.notify_one();
    }
}

fn define_wasi(linker: &mut Linker<HostState>) -> Result<(), String> {
    linker
        .func_wrap(WASI_MODULE, "fd_write", wasi_fd_write)
        .map_err(|e| format!("linking wasi fd_write: {e}"))?;
    linker
        .func_wrap(WASI_MODULE, "environ_sizes_get", wasi_sizes_get)
        .map_err(|e| format!("linking wasi environ_sizes_get: {e}"))?;
    linker
        .func_wrap(WASI_MODULE, "environ_get", |_environ: i32, _buf: i32| {
            ERRNO_SUCCESS
        })
        .map_err(|e| format!("linking wasi environ_get: {e}"))?;
    linker
        .func_wrap(WASI_MODULE, "args_sizes_get", wasi_sizes_get)
        .map_err(|e| format!("linking wasi args_sizes_get: {e}"))?;
    linker
        .func_wrap(WASI_MODULE, "args_get", |_argv: i32, _buf: i32| {
            ERRNO_SUCCESS
        })
        .map_err(|e| format!("linking wasi args_get: {e}"))?;
    linker
        .func_wrap(WASI_MODULE, "clock_time_get", wasi_clock_time_get)
        .map_err(|e| format!("linking wasi clock_time_get: {e}"))?;
    linker
        .func_wrap(WASI_MODULE, "random_get", wasi_random_get)
        .map_err(|e| format!("linking wasi random_get: {e}"))?;
    linker
        .func_wrap(
            WASI_MODULE,
            "proc_exit",
            |code: i32| -> Result<(), wasmtime::Error> {
                Err(wasmtime::Error::msg(format!("guest proc_exit({code})")))
            },
        )
        .map_err(|e| format!("linking wasi proc_exit: {e}"))?;
    linker
        .func_wrap(WASI_MODULE, "fd_close", |_fd: i32| ERRNO_SUCCESS)
        .map_err(|e| format!("linking wasi fd_close: {e}"))?;
    linker
        .func_wrap(WASI_MODULE, "fd_fdstat_get", wasi_fd_fdstat_get)
        .map_err(|e| format!("linking wasi fd_fdstat_get: {e}"))?;
    linker
        .func_wrap(WASI_MODULE, "sched_yield", || ERRNO_SUCCESS)
        .map_err(|e| format!("linking wasi sched_yield: {e}"))?;
    Ok(())
}

fn wasi_sizes_get(mut caller: Caller<'_, HostState>, count_ptr: i32, size_ptr: i32) -> i32 {
    let first = write_u32(&mut caller, count_ptr, 0);
    let second = write_u32(&mut caller, size_ptr, 0);
    if first == ERRNO_SUCCESS {
        second
    } else {
        first
    }
}

fn wasi_clock_time_get(
    mut caller: Caller<'_, HostState>,
    _clock_id: i32,
    _precision: i64,
    time_ptr: i32,
) -> i32 {
    write_u64(&mut caller, time_ptr, 0)
}

fn wasi_random_get(mut caller: Caller<'_, HostState>, ptr: i32, len: i32) -> i32 {
    if ptr < 0 || len < 0 {
        return ERRNO_INVAL;
    }
    let memory = match guest_memory(&mut caller) {
        Ok(memory) => memory,
        Err(_) => return ERRNO_INVAL,
    };
    let bytes = vec![0x42; len as usize];
    match memory.write(&mut caller, ptr as usize, &bytes) {
        Ok(()) => ERRNO_SUCCESS,
        Err(_) => ERRNO_INVAL,
    }
}

fn wasi_fd_fdstat_get(mut caller: Caller<'_, HostState>, _fd: i32, stat_ptr: i32) -> i32 {
    if stat_ptr < 0 {
        return ERRNO_INVAL;
    }
    let memory = match guest_memory(&mut caller) {
        Ok(memory) => memory,
        Err(_) => return ERRNO_INVAL,
    };
    let stat = [0_u8; 24];
    match memory.write(&mut caller, stat_ptr as usize, &stat) {
        Ok(()) => ERRNO_SUCCESS,
        Err(_) => ERRNO_INVAL,
    }
}

fn wasi_fd_write(
    mut caller: Caller<'_, HostState>,
    fd: i32,
    iovs: i32,
    iovs_len: i32,
    nwritten: i32,
) -> i32 {
    if fd != 1 && fd != 2 {
        return ERRNO_BADF;
    }
    if iovs < 0 || iovs_len < 0 {
        return ERRNO_INVAL;
    }
    let memory = match guest_memory(&mut caller) {
        Ok(memory) => memory,
        Err(_) => return ERRNO_INVAL,
    };
    let data = memory.data(&caller);
    let mut out = Vec::new();
    for index in 0..iovs_len as usize {
        let offset = iovs as usize + index * 8;
        let end = offset.saturating_add(8);
        if end > data.len() {
            return ERRNO_INVAL;
        }
        let ptr = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        let len = u32::from_le_bytes([
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]) as usize;
        let end = ptr.saturating_add(len);
        if end > data.len() {
            return ERRNO_INVAL;
        }
        out.extend_from_slice(&data[ptr..end]);
    }
    let written = out.len();
    write_guest_stderr(&out);
    write_u32(&mut caller, nwritten, written as u32)
}

fn write_guest_stderr(bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    for part in text.split_inclusive('\n') {
        eprint!("guest: {part}");
    }
    if !text.ends_with('\n') && !text.is_empty() {
        eprintln!();
    }
}

fn write_u32(caller: &mut Caller<'_, HostState>, ptr: i32, value: u32) -> i32 {
    if ptr < 0 {
        return ERRNO_INVAL;
    }
    let memory = match guest_memory(caller) {
        Ok(memory) => memory,
        Err(_) => return ERRNO_INVAL,
    };
    match memory.write(caller, ptr as usize, &value.to_le_bytes()) {
        Ok(()) => ERRNO_SUCCESS,
        Err(_) => ERRNO_INVAL,
    }
}

fn write_u64(caller: &mut Caller<'_, HostState>, ptr: i32, value: u64) -> i32 {
    if ptr < 0 {
        return ERRNO_INVAL;
    }
    let memory = match guest_memory(caller) {
        Ok(memory) => memory,
        Err(_) => return ERRNO_INVAL,
    };
    match memory.write(caller, ptr as usize, &value.to_le_bytes()) {
        Ok(()) => ERRNO_SUCCESS,
        Err(_) => ERRNO_INVAL,
    }
}

fn parse_object(bytes: &[u8]) -> Result<(String, Vec<u8>), String> {
    let nul = bytes
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| "object response missing NUL after header".to_string())?;
    let header =
        std::str::from_utf8(&bytes[..nul]).map_err(|e| format!("bad object header: {e}"))?;
    let content = &bytes[nul + 1..];
    let (kind, size) = header
        .split_once(' ')
        .ok_or_else(|| "bad object header: expected '<type> <size>'".to_string())?;
    let size: usize = size.parse().map_err(|e| format!("bad object size: {e}"))?;
    if size != content.len() {
        return Err(format!(
            "object size {size} != content length {}",
            content.len()
        ));
    }
    Ok((kind.to_string(), content.to_vec()))
}

fn parse_tree_entries(hash: &str, content: &[u8]) -> Result<Vec<TreeEntry>, String> {
    let tree = gix::objs::TreeRef::from_bytes(content, gix::hash::Kind::Sha1)
        .map_err(|e| format!("malformed tree {hash}: {e}"))?;
    Ok(tree
        .entries
        .iter()
        .map(|entry| TreeEntry {
            name: String::from_utf8_lossy(entry.filename).into_owned(),
            kind: if entry.mode.is_tree() {
                "tree".to_string()
            } else {
                "blob".to_string()
            },
            hash: entry.oid.to_string(),
            mode: Some(mode_to_string(entry.mode.kind()).to_string()),
        })
        .collect())
}

fn encode_tree_entries(entries: &[TreeEntry]) -> Result<Vec<u8>, String> {
    let mut encoded = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.name.is_empty() || entry.name.contains('/') || entry.name.as_bytes().contains(&0) {
            return Err(format!("invalid tree entry name {:?}", entry.name));
        }
        validate_hash(&entry.hash)?;
        encoded.push(gix::objs::tree::Entry {
            mode: entry_mode(entry)?,
            filename: entry.name.as_bytes().to_vec().into(),
            oid: parse_oid(&entry.hash)?,
        });
    }
    encoded.sort();
    let mut bytes = Vec::new();
    gix::objs::Tree { entries: encoded }
        .write_to(&mut bytes)
        .map_err(|e| format!("encoding tree: {e}"))?;
    Ok(bytes)
}

fn entry_mode(entry: &TreeEntry) -> Result<gix::objs::tree::EntryMode, String> {
    use gix::objs::tree::EntryKind;
    let kind = match entry.mode.as_deref() {
        Some("40000") | Some("040000") => EntryKind::Tree,
        Some("100644") => EntryKind::Blob,
        Some("100755") => EntryKind::BlobExecutable,
        Some("120000") => EntryKind::Link,
        Some("160000") => EntryKind::Commit,
        Some(other) => return Err(format!("unsupported tree entry mode {other:?}")),
        None if entry.kind == "tree" => EntryKind::Tree,
        None if entry.kind == "blob" => EntryKind::Blob,
        None => return Err(format!("unsupported tree entry kind {:?}", entry.kind)),
    };
    Ok(kind.into())
}

fn mode_to_string(kind: gix::objs::tree::EntryKind) -> &'static str {
    use gix::objs::tree::EntryKind;
    match kind {
        EntryKind::Tree => "40000",
        EntryKind::Blob => "100644",
        EntryKind::BlobExecutable => "100755",
        EntryKind::Link => "120000",
        EntryKind::Commit => "160000",
    }
}

fn build_request(
    ops: &dyn Ops,
    image: &str,
    args_tree: &str,
    std: &str,
    salt: &str,
) -> Result<String, String> {
    validate_hash(args_tree)?;
    if !std.is_empty() {
        validate_hash(std)?;
    }
    let entries = vec![
        TreeEntry {
            name: "image".to_string(),
            kind: "blob".to_string(),
            hash: ops.put_blob(image.as_bytes())?,
            mode: None,
        },
        TreeEntry {
            name: "args".to_string(),
            kind: "tree".to_string(),
            hash: args_tree.to_string(),
            mode: None,
        },
        TreeEntry {
            name: "std".to_string(),
            kind: "blob".to_string(),
            hash: ops.put_blob(std.as_bytes())?,
            mode: None,
        },
        TreeEntry {
            name: "salt".to_string(),
            kind: "blob".to_string(),
            hash: ops.put_blob(salt.as_bytes())?,
            mode: None,
        },
    ];
    ops.put_tree(&entries)
}

fn resolve_curry(
    ops: &dyn Ops,
    std: &str,
    image: &str,
    args: &str,
) -> Result<(String, String), String> {
    let (image, bound) = unwrap_curry(ops, std, image)?;
    if bound.is_empty() {
        return Ok((image, args.to_string()));
    }
    let call = ops.tree(args)?;
    let args_tree = ops.put_tree(&merge_entries(bound, call))?;
    Ok((image, args_tree))
}

/// Resolve a `/cas/std/<name>` image ref against the job's std tree. The
/// container worker's `caos run` does this against the materialized /cas/std;
/// an isolate has no /cas, so its host resolves the same way — refs entering
/// requests as hex keeps request hashes (and so cache keys) aligned with
/// container workers.
fn resolve_std_ref(ops: &dyn Ops, std: &str, image: &str) -> Result<String, String> {
    let Some(name) = image.strip_prefix("/cas/std/") else {
        return Ok(image.to_string());
    };
    if std.is_empty() {
        return Err(format!("cannot resolve {image}: job carries no std"));
    }
    ops.tree(std)?
        .into_iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.hash)
        .ok_or_else(|| format!("no builtin {name:?} in std tree {std}"))
}

fn unwrap_curry(ops: &dyn Ops, std: &str, image: &str) -> Result<(String, Vec<TreeEntry>), String> {
    let mut image = resolve_std_ref(ops, std, image)?;
    let mut bound = Vec::new();
    while is_hex_hash(&image) {
        match curry_node(ops, &image)? {
            Some((inner_image, inner_args)) => {
                bound = merge_entries(inner_args, bound);
                image = resolve_std_ref(ops, std, &inner_image)?;
            }
            None => break,
        }
    }
    Ok((image, bound))
}

fn curry_node(ops: &dyn Ops, image: &str) -> Result<Option<(String, Vec<TreeEntry>)>, String> {
    let entries = ops.tree(image)?;
    if !entries.iter().any(|entry| entry.name == CURRY_MARKER) {
        return Ok(None);
    }
    let base = entries
        .iter()
        .find(|entry| entry.name == "base")
        .ok_or_else(|| format!("curry node {image} missing base"))?;
    let args = entries
        .iter()
        .find(|entry| entry.name == "args")
        .ok_or_else(|| format!("curry node {image} missing args"))?;
    let base_bytes = ops.get(&base.hash)?;
    let base = std::str::from_utf8(&base_bytes)
        .map_err(|e| format!("curry node {image} base is not UTF-8: {e}"))?
        .trim()
        .to_string();
    let args = ops.tree(&args.hash)?;
    Ok(Some((base, args)))
}

fn merge_entries(low: Vec<TreeEntry>, high: Vec<TreeEntry>) -> Vec<TreeEntry> {
    let mut by_name: BTreeMap<String, TreeEntry> = BTreeMap::new();
    for entry in low.into_iter().chain(high) {
        by_name.insert(entry.name.clone(), entry);
    }
    by_name.into_values().collect()
}

fn parse_run_result(text: &str) -> Result<RunResult, String> {
    let (kind, hash) = text
        .trim()
        .split_once(' ')
        .ok_or_else(|| format!("malformed run result: {text:?}"))?;
    validate_result_kind(kind)?;
    validate_hash(hash)?;
    Ok(RunResult {
        kind: kind.to_string(),
        hash: hash.to_string(),
    })
}

fn tree_entry_to_json(entry: &TreeEntry) -> Value {
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

fn parse_json_entries(value: &Value) -> Result<Vec<TreeEntry>, String> {
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "put_tree missing array 'entries'".to_string())?;
    let mut parsed = Vec::with_capacity(entries.len());
    for entry in entries {
        parsed.push(TreeEntry {
            name: json_string(entry, "name")?.to_string(),
            kind: json_string(entry, "kind")?.to_string(),
            hash: json_string(entry, "hash")?.to_string(),
            mode: json_string_opt(entry, "mode")?,
        });
    }
    Ok(parsed)
}

fn json_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string '{key}'"))
}

fn json_string_opt(value: &Value, key: &str) -> Result<Option<String>, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("field '{key}' must be a string")),
    }
}

fn validate_result_kind(kind: &str) -> Result<(), String> {
    match kind {
        "blob" | "tree" => Ok(()),
        other => Err(format!("invalid result kind {other:?}")),
    }
}

fn validate_hash(hash: &str) -> Result<(), String> {
    if is_hex_hash(hash) {
        Ok(())
    } else {
        Err(format!("invalid git SHA-1 hash {hash:?}"))
    }
}

fn is_hex_hash(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn parse_oid(hex: &str) -> Result<gix::ObjectId, String> {
    gix::ObjectId::from_hex(hex.as_bytes()).map_err(|e| format!("invalid hash {hex:?}: {e}"))
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    struct MockOps;

    impl Ops for MockOps {
        fn tree(&self, _hash: &str) -> Result<Vec<TreeEntry>, String> {
            Err("unexpected tree".to_string())
        }

        fn get(&self, _hash: &str) -> Result<Vec<u8>, String> {
            Err("unexpected get".to_string())
        }

        fn put_blob(&self, _bytes: &[u8]) -> Result<String, String> {
            Err("unexpected put_blob".to_string())
        }

        fn put_tree(&self, _entries: &[TreeEntry]) -> Result<String, String> {
            Err("unexpected put_tree".to_string())
        }

        fn run(&self, _job: &Job, _image: &str, _args: &str) -> Result<RunResult, String> {
            Err("unexpected run".to_string())
        }
    }

    #[test]
    fn smoke_still_works() {
        smoke().expect("smoke");
    }

    #[test]
    fn wasm_job_to_out_plumbing() {
        let job_op = br#"{"op":"job"}"#;
        let out_op =
            br#"{"op":"out","kind":"blob","hash":"1111111111111111111111111111111111111111"}"#;
        let wat = format!(
            r#"
            (module
              (import "caos_abi_v1" "call" (func $call (param i32 i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 0) "{}")
              (data (i32.const 128) "{}")
              (func (export "caos_run")
                i32.const 0
                i32.const {}
                call $call
                drop
                i32.const 128
                i32.const {}
                call $call
                drop))
            "#,
            wat_string(job_op),
            wat_string(out_op),
            job_op.len(),
            out_op.len()
        );
        let engine = engine().expect("engine");
        let module = Module::new(&engine, wat).expect("module");
        let job = Job {
            args: "2222222222222222222222222222222222222222".to_string(),
            std: "3333333333333333333333333333333333333333".to_string(),
            salt: "salt".to_string(),
            stack: "stack".to_string(),
        };
        let execution = run_module(&module, Arc::new(MockOps), job).expect("run module");
        assert_eq!(
            execution.result,
            RunResult {
                kind: "blob".to_string(),
                hash: "1111111111111111111111111111111111111111".to_string()
            }
        );
    }

    // Pinned against real git (the nix check sandbox has no git to shell out
    // to). Fixture: a repo holding a.txt="alpha", b.txt="bravo", link->a.txt,
    // sub/z.txt="zulu"; `git write-tree` gives eaf6c7722e5ecf58ef9e0d3085dea126
    // 76a0ee83 and `git cat-file tree` gives exactly these bytes. Entries are
    // fed unsorted and with mixed mode/kind spellings to cover the encoder's
    // sorting and mode normalization.
    #[test]
    fn tree_encoding_matches_git_write_tree() {
        let entries = vec![
            TreeEntry {
                name: "sub".to_string(),
                kind: "tree".to_string(),
                hash: "72faf5f78a2e8c40511ebd116035a8dc8d7e1f62".to_string(),
                mode: None,
            },
            TreeEntry {
                name: "b.txt".to_string(),
                kind: "blob".to_string(),
                hash: "9c7b8743eaa88e1b7afa05d7e3df7db02eb78aa4".to_string(),
                mode: None,
            },
            TreeEntry {
                name: "link".to_string(),
                kind: "blob".to_string(),
                hash: "8d14cbf983b3fad683171c9418998d9f68340823".to_string(),
                mode: Some("120000".to_string()),
            },
            TreeEntry {
                name: "a.txt".to_string(),
                kind: "blob".to_string(),
                hash: "7e74e68b2a782a3aead46d987a63ca1c91091c13".to_string(),
                mode: Some("100644".to_string()),
            },
        ];
        let encoded = encode_tree_entries(&entries).expect("encode tree");
        let expected = concat!(
            "31303036343420612e747874007e74e68b2a782a3aead46d987a63ca1c91091c13",
            "31303036343420622e747874009c7b8743eaa88e1b7afa05d7e3df7db02eb78aa4",
            "313230303030206c696e6b008d14cbf983b3fad683171c9418998d9f68340823",
            "3430303030207375620072faf5f78a2e8c40511ebd116035a8dc8d7e1f62"
        );
        assert_eq!(hex_encode(&encoded), expected);
    }

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn wat_string(bytes: &[u8]) -> String {
        let mut out = String::new();
        for &byte in bytes {
            match byte {
                b'"' => out.push_str("\\\""),
                b'\\' => out.push_str("\\\\"),
                0x20..=0x7e => out.push(byte as char),
                _ => out.push_str(&format!("\\{byte:02x}")),
            }
        }
        out
    }
}
