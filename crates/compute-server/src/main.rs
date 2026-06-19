//! compute-server: run a containerized compute step and return its result hash.
//!
//! One endpoint:
//!
//! * `GET /run?image=<image>&args=<hash>` — run `<image>` over the args tree
//!   `<hash>` and return the hash of its result.
//!
//! It shells out to the `docker` CLI:
//!
//! ```text
//! docker run --rm --network <net> \
//!     -e CAOS_OBJECT_SERVER_URL=<url> -e CAOS_COMPUTE_SERVER_URL=<url> \
//!     --entrypoint /bin/caos <image> entrypoint --args=<hash>
//! ```
//!
//! Forcing `--entrypoint /bin/caos` means any image carrying the `caos` binary
//! and a `/worker` works as a compute image, regardless of its own configured
//! entrypoint/command. `caos entrypoint` populates `/cas/args` from `<hash>`,
//! runs `/worker`, and prints the hash recorded at `/cas/out` on its stdout —
//! which `docker run` forwards to ours, so the container's stdout *is* the
//! result hash. We return it as the response body.
//!
//! Both daemon URLs are injected so the worker can reach the object server and —
//! for a worker that itself calls `caos run`, like the fold worker — call back
//! into us. The container reaches both over the Docker network, so it must be the
//! same network the daemons run on (default `caos-net`).
//!
//! Results are cached in Redis (`CAOS_REDIS_ADDR`, default `caos-redis:6379`):
//! the key is the image + args-tree hash, the value the result hash. A hit skips
//! the container entirely. Redis is best-effort — if it's unreachable we log and
//! run uncached.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tiny_http::{Method, Request, Response, Server};

/// Listen address; overridable for local runs outside the container.
const DEFAULT_ADDR: &str = "0.0.0.0:80";

/// Docker network the worker container joins, so the object server resolves by
/// name. Override with `CAOS_DOCKER_NETWORK`.
const DEFAULT_NETWORK: &str = "caos-net";

/// Object-server URL passed into the worker container. Override with
/// `CAOS_OBJECT_SERVER_URL`.
const DEFAULT_OBJECT_SERVER_URL: &str = "http://caos-object-server";

/// Compute-server URL passed into the worker container, so a worker that calls
/// `caos run` (e.g. the fold worker, which recurses) can reach us. This is our
/// own address as seen from inside the Docker network. Override with
/// `CAOS_COMPUTE_SERVER_URL`.
const DEFAULT_COMPUTE_SERVER_URL: &str = "http://caos-compute-server";

/// Registry base URL converted git-docker images are pushed to, reachable from
/// *this* container over the docker network. Override with
/// `CAOS_REGISTRY_PUSH_URL`.
const DEFAULT_REGISTRY_PUSH_URL: &str = "http://caos-registry:5000";

/// How the host's docker daemon (which actually runs the worker) refers to that
/// same registry — a published port on localhost, which docker treats as an
/// insecure registry, so no TLS/daemon config is needed. Override with
/// `CAOS_REGISTRY_PULL_HOST`.
const DEFAULT_REGISTRY_PULL_HOST: &str = "localhost:5000";

/// Repository name converted images are pushed under. They're addressed by
/// digest, so the name is arbitrary and fixed.
const REGISTRY_REPO: &str = "caos";

/// Prefix marking the `image` parameter as an ordinary docker reference rather
/// than one of our git images (the default).
const DOCKER_SCHEME: &str = "docker://";

/// `docker` binary to invoke. Override with `CAOS_DOCKER_BIN`.
const DEFAULT_DOCKER_BIN: &str = "docker";

/// Redis (host:port) used to cache results. Override with `CAOS_REDIS_ADDR`.
const DEFAULT_REDIS_ADDR: &str = "caos-redis:6379";

/// How long to wait on Redis before giving up and running uncached.
const REDIS_TIMEOUT: Duration = Duration::from_secs(5);

/// The caos binary inside every compute image, forced as the entrypoint.
const CAOS_BIN: &str = "/bin/caos";

/// Runtime configuration, read once from the environment at startup.
struct Config {
    network: String,
    object_server_url: String,
    compute_server_url: String,
    registry_push_url: String,
    registry_pull_host: String,
    docker_bin: String,
    redis_addr: String,
}

/// Install handlers so the process terminates on `SIGINT`/`SIGTERM`. This matters
/// in a container, where the daemon is PID 1: the kernel applies no default
/// disposition for these signals to PID 1, so without an explicit handler
/// `docker stop` (and Tilt's Ctrl-C) would hang until the 10s `SIGKILL`.
fn install_termination_handlers() {
    // Async-signal-safe: we hold no state that needs flushing, so just exit.
    extern "C" fn terminate(_signum: std::ffi::c_int) {
        unsafe { exit_now(0) }
    }
    extern "C" {
        // libc, resolved against what std already links.
        fn signal(signum: std::ffi::c_int, handler: extern "C" fn(std::ffi::c_int)) -> usize;
        #[link_name = "_exit"]
        fn exit_now(code: std::ffi::c_int) -> !;
    }
    const SIGINT: std::ffi::c_int = 2;
    const SIGTERM: std::ffi::c_int = 15;
    unsafe {
        signal(SIGINT, terminate);
        signal(SIGTERM, terminate);
    }
}

fn main() {
    install_termination_handlers();

    let addr = std::env::var("COMPUTE_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    // Shared read-only across handler threads (one per request, see below).
    let config = Arc::new(Config {
        network: env_or("CAOS_DOCKER_NETWORK", DEFAULT_NETWORK),
        object_server_url: env_or("CAOS_OBJECT_SERVER_URL", DEFAULT_OBJECT_SERVER_URL),
        compute_server_url: env_or("CAOS_COMPUTE_SERVER_URL", DEFAULT_COMPUTE_SERVER_URL),
        registry_push_url: env_or("CAOS_REGISTRY_PUSH_URL", DEFAULT_REGISTRY_PUSH_URL),
        registry_pull_host: env_or("CAOS_REGISTRY_PULL_HOST", DEFAULT_REGISTRY_PULL_HOST),
        docker_bin: env_or("CAOS_DOCKER_BIN", DEFAULT_DOCKER_BIN),
        redis_addr: env_or("CAOS_REDIS_ADDR", DEFAULT_REDIS_ADDR),
    });

    let server = match Server::http(addr.as_str()) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("fatal: cannot bind {addr}: {err}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "compute-server listening on http://{addr}, network {}, object server {}, \
         compute server {}, registry push {} / pull {}, redis {}",
        config.network,
        config.object_server_url,
        config.compute_server_url,
        config.registry_push_url,
        config.registry_pull_host,
        config.redis_addr
    );

    // One thread per request, not a serial loop: a worker can itself call back
    // into us (the fold worker recurses via `caos run`), and that nested request
    // must be served while its parent's request is still blocked waiting on the
    // `docker run` it spawned. A serial loop — or any pool smaller than the tree
    // is deep — would deadlock. Threads are cheap here: each just blocks in
    // `docker run`'s `waitpid`.
    for request in server.incoming_requests() {
        let config = Arc::clone(&config);
        std::thread::spawn(move || {
            if let Err(err) = handle(&config, request) {
                // Only reachable if writing the response itself fails.
                eprintln!("failed to send response: {err}");
            }
        });
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// An error that maps cleanly onto an HTTP status code + body.
struct HttpError {
    status: u16,
    message: String,
}

impl HttpError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

/// Dispatch a single request and send its response.
fn handle(config: &Config, request: Request) -> std::io::Result<()> {
    match route(config, &request) {
        Ok(body) => request.respond(Response::from_data(body)),
        Err(err) => request.respond(
            Response::from_string(format!("{}\n", err.message))
                .with_status_code(tiny_http::StatusCode(err.status)),
        ),
    }
}

/// Match the request to a handler and produce the response body.
fn route(config: &Config, request: &Request) -> Result<Vec<u8>, HttpError> {
    let url = request.url();
    let (path, query) = url.split_once('?').unwrap_or((url, ""));

    match (request.method(), path) {
        (Method::Get, "/run") => run(config, query),
        _ => Err(HttpError::new(404, "not found")),
    }
}

/// `GET /run?image=<image>&args=<hash>` — run the image and return its result.
fn run(config: &Config, query: &str) -> Result<Vec<u8>, HttpError> {
    let image = query_param(query, "image")
        .ok_or_else(|| HttpError::new(400, "missing 'image' query parameter"))?;
    let args = query_param(query, "args")
        .ok_or_else(|| HttpError::new(400, "missing 'args' query parameter"))?;

    if image.is_empty() {
        return Err(HttpError::new(400, "empty image"));
    }
    // The args hash is interpolated into `--args=`; require a plain hex object id.
    if args.is_empty() || !args.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HttpError::new(400, format!("invalid args hash: {args:?}")));
    }

    // Cache key is the image + args-tree hash; value is the result hash. Keying on
    // the image param as given (a git hash, or a `docker://` ref) means a hit
    // skips both image conversion and the container run. Redis is best-effort: a
    // lookup/connection error just means we run uncached.
    let key = format!("caos:result:{image}\0{args}");
    match cache_get(&config.redis_addr, &key) {
        Ok(Some(result)) => {
            eprintln!("cache hit: image={image} args={args} -> {result}");
            return Ok(format!("{result}\n").into_bytes());
        }
        Ok(None) => eprintln!("cache miss: image={image} args={args}; running worker"),
        Err(e) => eprintln!("cache lookup failed ({e}); running worker: image={image} args={args}"),
    }

    // Resolve to a reference the host's docker daemon can run: a `docker://`
    // image is used directly; one of our git images is converted to a real image,
    // pushed to the registry, and referenced by digest.
    let docker_ref = resolve_image(config, &image)?;

    let output = Command::new(&config.docker_bin)
        .arg("run")
        .arg("--rm")
        .args(["--network", &config.network])
        .args([
            "-e",
            &format!("CAOS_OBJECT_SERVER_URL={}", config.object_server_url),
        ])
        .args([
            "-e",
            &format!("CAOS_COMPUTE_SERVER_URL={}", config.compute_server_url),
        ])
        .args(["--entrypoint", CAOS_BIN])
        .arg(&docker_ref)
        .arg("entrypoint")
        .arg(format!("--args={args}"))
        .output()
        .map_err(|e| HttpError::new(500, format!("running {}: {e}", config.docker_bin)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!(
            "worker failed: image={image} args={args} ({}):\n{}",
            output.status,
            stderr.trim_end()
        );
        return Err(HttpError::new(
            500,
            format!("worker container failed ({}):\n{stderr}", output.status),
        ));
    }

    // The container's stdout is the result hash printed by `caos entrypoint`.
    let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if hash.is_empty() {
        eprintln!("worker produced no result hash on stdout: image={image} args={args}");
        return Err(HttpError::new(
            500,
            "worker container produced no result hash on stdout",
        ));
    }

    // Cache the result for next time (best-effort).
    match cache_set(&config.redis_addr, &key, &hash) {
        Ok(()) => eprintln!("ran worker: image={image} args={args} -> {hash} (cached)"),
        Err(e) => {
            eprintln!("ran worker: image={image} args={args} -> {hash} (cache store failed: {e})")
        }
    }

    Ok(format!("{hash}\n").into_bytes())
}

/// Disambiguates temp dirs created across handler threads.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Resolve the `image` parameter to a reference the host docker daemon can run.
///
/// `docker://<ref>` is an ordinary docker reference, used as-is. Anything else is
/// one of our git images (the default): convert it to a real image, push it to
/// the registry, and return a digest reference into the registry.
fn resolve_image(config: &Config, image: &str) -> Result<String, HttpError> {
    if let Some(reference) = image.strip_prefix(DOCKER_SCHEME) {
        if reference.is_empty() || reference.starts_with('-') {
            return Err(HttpError::new(
                400,
                format!("invalid docker image: {reference:?}"),
            ));
        }
        return Ok(reference.to_string());
    }
    if !image.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HttpError::new(
            400,
            format!("git image must be a hex hash (or use {DOCKER_SCHEME}<ref>): {image:?}"),
        ));
    }
    convert_git_image(config, image)
        .map_err(|e| HttpError::new(500, format!("converting git image {image}: {e}")))
}

/// A git tree entry, owned so it outlives the fetched object bytes.
struct TreeEntry {
    name: String,
    mode: gix::objs::tree::EntryMode,
    oid: gix::ObjectId,
}

/// Convert the git-docker image tree `git_hash` to a real image and push it to
/// the registry, returning a digest reference. Cached in Redis by git hash.
fn convert_git_image(config: &Config, git_hash: &str) -> Result<String, String> {
    let image_key = format!("caos:image:{git_hash}");
    if let Ok(Some(manifest_digest)) = cache_get(&config.redis_addr, &image_key) {
        eprintln!("image cache hit: {git_hash} -> {manifest_digest}");
        return Ok(image_ref(config, &manifest_digest));
    }

    // The image tree holds `config.json` (a blob) and `layer<NN>` subtrees.
    let mut config_oid: Option<String> = None;
    let mut layers: Vec<(u64, String)> = Vec::new();
    for entry in fetch_tree(config, git_hash)? {
        if entry.name == "config.json" {
            config_oid = Some(entry.oid.to_string());
        } else if let Some(suffix) = entry.name.strip_prefix("layer") {
            // layer<NN>: number it for ordering (matches config.rootfs.diff_ids).
            if let Ok(num) = suffix.parse::<u64>() {
                if !entry.mode.is_tree() {
                    return Err(format!("layer entry {} is not a directory", entry.name));
                }
                layers.push((num, entry.oid.to_string()));
            }
        }
    }
    let config_oid = config_oid.ok_or("image tree has no config.json")?;
    if layers.is_empty() {
        return Err("image tree has no layer<NN> entries".to_string());
    }
    layers.sort_by_key(|(num, _)| *num);

    // Each layer becomes an uncompressed tar; since it's uncompressed, the blob
    // digest and the config's diff_id are the same sha256.
    let mut layer_descs: Vec<(String, u64)> = Vec::new();
    let mut diff_ids: Vec<String> = Vec::new();
    for (_, oid) in &layers {
        let (digest, size) = ensure_layer(config, oid)?;
        diff_ids.push(digest.clone());
        layer_descs.push((digest, size));
    }

    // Set the config's diff_ids to the layers we just built, so the image is
    // self-consistent. We generate them outright — the stored config needn't
    // carry diff_ids (the producer can't know them without tarring).
    let config_bytes = fetch_blob(config, &config_oid)?;
    let new_config = set_config_diff_ids(&config_bytes, &diff_ids)?;
    let config_digest = format!("sha256:{}", sha256_hex(&new_config));
    push_blob(config, &config_digest, &new_config)?;

    let manifest = build_manifest(&config_digest, new_config.len() as u64, &layer_descs);
    let manifest_bytes =
        serde_json::to_vec(&manifest).map_err(|e| format!("serializing manifest: {e}"))?;
    let manifest_digest = format!("sha256:{}", sha256_hex(&manifest_bytes));
    push_manifest(config, &manifest_digest, &manifest_bytes)?;

    let _ = cache_set(&config.redis_addr, &image_key, &manifest_digest);
    eprintln!("converted image {git_hash} -> {manifest_digest}");
    Ok(image_ref(config, &manifest_digest))
}

/// The digest reference the host daemon uses to pull the converted image.
fn image_ref(config: &Config, manifest_digest: &str) -> String {
    format!(
        "{}/{REGISTRY_REPO}@{manifest_digest}",
        config.registry_pull_host.trim_end_matches('/')
    )
}

/// Build (if not cached) and push the layer whose git tree is `layer_oid`,
/// returning its `(digest, size)`. The git-hash → digest+size mapping is cached
/// in Redis so an unchanged layer is never re-tarred or re-pushed.
fn ensure_layer(config: &Config, layer_oid: &str) -> Result<(String, u64), String> {
    let key = format!("caos:layer:{layer_oid}");
    if let Ok(Some(value)) = cache_get(&config.redis_addr, &key) {
        if let Some((digest, size)) = value.split_once(' ') {
            if let Ok(size) = size.parse::<u64>() {
                eprintln!("layer cache hit: {layer_oid} -> {digest}");
                return Ok((digest.to_string(), size));
            }
        }
    }
    let tar = build_layer_tar(config, layer_oid)?;
    let digest = format!("sha256:{}", sha256_hex(&tar));
    let size = tar.len() as u64;
    push_blob(config, &digest, &tar)?;
    let _ = cache_set(&config.redis_addr, &key, &format!("{digest} {size}"));
    eprintln!("converted layer {layer_oid} -> {digest} ({size} bytes)");
    Ok((digest, size))
}

/// Materialize a layer's git tree to a temp dir and tar it deterministically
/// (GNU format handles the long /nix/store paths and symlinks; the flags zero out
/// owners/mtimes and sort entries, so the output — hence its digest — is stable).
fn build_layer_tar(config: &Config, tree_hash: &str) -> Result<Vec<u8>, String> {
    let dir = temp_dir()?;
    let result = (|| {
        materialize_tree(config, &dir, tree_hash)?;
        tar_dir(&dir)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// A fresh, unique temp directory.
fn temp_dir() -> Result<PathBuf, String> {
    let base = std::env::temp_dir().join("caos-convert");
    std::fs::create_dir_all(&base).map_err(|e| format!("creating {}: {e}", base.display()))?;
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!("{}-{n}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Write a git tree's contents into `dir`: files (with their exec bit), symlinks,
/// and subdirectories, recursively. Modes are set explicitly so the tar is
/// independent of the umask.
fn materialize_tree(config: &Config, dir: &Path, tree_hash: &str) -> Result<(), String> {
    use gix::objs::tree::EntryKind;
    for entry in fetch_tree(config, tree_hash)? {
        let path = dir.join(&entry.name);
        match entry.mode.kind() {
            EntryKind::Tree => {
                std::fs::create_dir(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                set_mode(&path, 0o755)?;
                materialize_tree(config, &path, &entry.oid.to_string())?;
            }
            EntryKind::Link => {
                let target = fetch_blob(config, &entry.oid.to_string())?;
                symlink(Path::new(std::ffi::OsStr::from_bytes(&target)), &path)
                    .map_err(|e| format!("symlink {}: {e}", path.display()))?;
            }
            EntryKind::Blob | EntryKind::BlobExecutable => {
                let content = fetch_blob(config, &entry.oid.to_string())?;
                std::fs::write(&path, content).map_err(|e| format!("{}: {e}", path.display()))?;
                let mode = if entry.mode.kind() == EntryKind::BlobExecutable {
                    0o755
                } else {
                    0o644
                };
                set_mode(&path, mode)?;
            }
            EntryKind::Commit => {
                return Err(format!("unexpected submodule entry: {}", entry.name));
            }
        }
    }
    Ok(())
}

/// Set a path's permission bits.
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {}: {e}", path.display()))
}

/// Tar `dir`'s contents reproducibly (GNU format, zeroed owners/mtimes, sorted).
fn tar_dir(dir: &Path) -> Result<Vec<u8>, String> {
    let output = Command::new("tar")
        .args([
            "--format=gnu",
            "--numeric-owner",
            "--owner=0",
            "--group=0",
            "--mtime=@0",
            "--sort=name",
        ])
        .arg("-C")
        .arg(dir)
        .args(["-cf", "-", "."])
        .output()
        .map_err(|e| format!("running tar: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "tar failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    Ok(output.stdout)
}

/// Set `rootfs.diff_ids` in the image config to `diff_ids` (in layer order),
/// creating `rootfs` if absent — we generate these outright rather than reading
/// any stored value, so the config needn't carry diff_ids (the producer can't
/// know them without tarring). Everything else in the config passes through;
/// other keys may be reordered by re-serialization, which is fine since we
/// compute the config digest from the result.
fn set_config_diff_ids(config_bytes: &[u8], diff_ids: &[String]) -> Result<Vec<u8>, String> {
    let mut value: serde_json::Value =
        serde_json::from_slice(config_bytes).map_err(|e| format!("parsing config.json: {e}"))?;
    let obj = value
        .as_object_mut()
        .ok_or("config.json is not a JSON object")?;
    let rootfs = obj.entry("rootfs").or_insert_with(|| serde_json::json!({}));
    let rootfs = rootfs
        .as_object_mut()
        .ok_or("config.json rootfs is not an object")?;
    rootfs.insert(
        "type".to_string(),
        serde_json::Value::String("layers".to_string()),
    );
    rootfs.insert(
        "diff_ids".to_string(),
        serde_json::Value::Array(
            diff_ids
                .iter()
                .map(|d| serde_json::Value::String(d.clone()))
                .collect(),
        ),
    );
    serde_json::to_vec(&value).map_err(|e| format!("serializing config.json: {e}"))
}

/// Build the OCI image manifest referencing the config and layer blobs.
fn build_manifest(
    config_digest: &str,
    config_size: u64,
    layers: &[(String, u64)],
) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_size,
        },
        "layers": layers.iter().map(|(digest, size)| serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": digest,
            "size": size,
        })).collect::<Vec<_>>(),
    })
}

/// Fetch and parse a git tree from the object server.
fn fetch_tree(config: &Config, hash: &str) -> Result<Vec<TreeEntry>, String> {
    let (kind, content) = fetch_object(config, hash)?;
    if kind != "tree" {
        return Err(format!("expected tree, got {kind} for {hash}"));
    }
    let tree = gix::objs::TreeRef::from_bytes(&content, gix::hash::Kind::Sha1)
        .map_err(|e| format!("malformed tree {hash}: {e}"))?;
    Ok(tree
        .entries
        .iter()
        .map(|e| TreeEntry {
            name: String::from_utf8_lossy(e.filename).into_owned(),
            mode: e.mode,
            oid: e.oid.to_owned(),
        })
        .collect())
}

/// Fetch a git blob's bytes from the object server.
fn fetch_blob(config: &Config, hash: &str) -> Result<Vec<u8>, String> {
    let (kind, content) = fetch_object(config, hash)?;
    if kind != "blob" {
        return Err(format!("expected blob, got {kind} for {hash}"));
    }
    Ok(content)
}

/// Fetch a git object from the object server, returning its `(type, content)`.
fn fetch_object(config: &Config, hash: &str) -> Result<(String, Vec<u8>), String> {
    let url = format!(
        "{}/object/{hash}",
        config.object_server_url.trim_end_matches('/')
    );
    let response = minreq::get(&url)
        .send()
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "GET {url}: server returned {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    let bytes = response.into_bytes();
    let nul = bytes
        .iter()
        .position(|&b| b == 0)
        .ok_or("object response missing NUL after header")?;
    let header =
        std::str::from_utf8(&bytes[..nul]).map_err(|e| format!("bad object header: {e}"))?;
    let (kind, size) = header
        .split_once(' ')
        .ok_or("bad object header: expected '<type> <size>'")?;
    let content = bytes[nul + 1..].to_vec();
    let size: usize = size.parse().map_err(|e| format!("bad object size: {e}"))?;
    if size != content.len() {
        return Err(format!(
            "object size {size} != content length {}",
            content.len()
        ));
    }
    Ok((kind.to_string(), content))
}

/// Upload a blob to the registry (monolithic two-step: start, then PUT bytes).
fn push_blob(config: &Config, digest: &str, data: &[u8]) -> Result<(), String> {
    let base = config.registry_push_url.trim_end_matches('/');
    let start = format!("{base}/v2/{REGISTRY_REPO}/blobs/uploads/");
    let response = minreq::post(&start)
        .send()
        .map_err(|e| format!("POST {start}: {e}"))?;
    if response.status_code != 202 {
        return Err(format!(
            "starting blob upload: {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    let location = response
        .headers
        .get("location")
        .ok_or("blob upload response missing Location")?
        .clone();
    let upload = if location.starts_with("http://") || location.starts_with("https://") {
        location
    } else {
        format!("{base}{location}")
    };
    let sep = if upload.contains('?') { '&' } else { '?' };
    let put = format!("{upload}{sep}digest={digest}");
    let response = minreq::put(&put)
        .with_header("Content-Type", "application/octet-stream")
        .with_body(data.to_vec())
        .send()
        .map_err(|e| format!("PUT {put}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "uploading blob {digest}: {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    Ok(())
}

/// Upload a manifest to the registry, addressed by its digest.
fn push_manifest(config: &Config, digest: &str, data: &[u8]) -> Result<(), String> {
    let base = config.registry_push_url.trim_end_matches('/');
    let url = format!("{base}/v2/{REGISTRY_REPO}/manifests/{digest}");
    let response = minreq::put(&url)
        .with_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
        .with_body(data.to_vec())
        .send()
        .map_err(|e| format!("PUT {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!(
            "uploading manifest {digest}: {} {}",
            response.status_code, response.reason_phrase
        ));
    }
    Ok(())
}

/// Hex sha256 of `data`.
fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `GET key` from Redis, returning the value or None if the key is absent.
fn cache_get(addr: &str, key: &str) -> Result<Option<String>, String> {
    let mut stream = redis_connect(addr)?;
    stream
        .write_all(&resp_command(&["GET", key]))
        .map_err(|e| format!("write: {e}"))?;
    read_bulk_reply(&mut BufReader::new(stream))
}

/// `SET key value` in Redis.
fn cache_set(addr: &str, key: &str, value: &str) -> Result<(), String> {
    let mut stream = redis_connect(addr)?;
    stream
        .write_all(&resp_command(&["SET", key, value]))
        .map_err(|e| format!("write: {e}"))?;
    read_status_reply(&mut BufReader::new(stream))
}

/// Connect to Redis with read/write timeouts so a stuck server can't hang us.
fn redis_connect(addr: &str) -> Result<TcpStream, String> {
    let stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    let _ = stream.set_read_timeout(Some(REDIS_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REDIS_TIMEOUT));
    Ok(stream)
}

/// Encode a Redis command as a RESP array of bulk strings (binary-safe, so the
/// NUL in our cache key is fine).
fn resp_command(args: &[&str]) -> Vec<u8> {
    let mut buf = format!("*{}\r\n", args.len()).into_bytes();
    for arg in args {
        buf.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        buf.extend_from_slice(arg.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

/// Read a RESP bulk-string reply (`$<len>\r\n<data>\r\n`); a nil reply (`$-1`)
/// becomes None and an error reply (`-...`) becomes Err.
fn read_bulk_reply(reader: &mut impl BufRead) -> Result<Option<String>, String> {
    let header = read_reply_line(reader)?;
    match header.as_bytes().first() {
        Some(b'$') => {
            let len: i64 = header[1..]
                .parse()
                .map_err(|e| format!("bad bulk length: {e}"))?;
            if len < 0 {
                return Ok(None); // nil
            }
            let mut buf = vec![0u8; len as usize + 2]; // data + trailing CRLF
            reader
                .read_exact(&mut buf)
                .map_err(|e| format!("read: {e}"))?;
            buf.truncate(len as usize);
            String::from_utf8(buf)
                .map(Some)
                .map_err(|e| format!("non-utf8 value: {e}"))
        }
        Some(b'-') => Err(format!("redis error: {}", &header[1..])),
        _ => Err(format!("unexpected reply: {header:?}")),
    }
}

/// Read a RESP simple-status reply (`+OK\r\n`); an error reply becomes Err.
fn read_status_reply(reader: &mut impl BufRead) -> Result<(), String> {
    let header = read_reply_line(reader)?;
    match header.as_bytes().first() {
        Some(b'+') => Ok(()),
        Some(b'-') => Err(format!("redis error: {}", &header[1..])),
        _ => Err(format!("unexpected reply: {header:?}")),
    }
}

/// Read one CRLF-terminated reply line, without the trailing CRLF.
fn read_reply_line(reader: &mut impl BufRead) -> Result<String, String> {
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?
        == 0
    {
        return Err("redis closed the connection".to_string());
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Find `name` in an `a=b&c=d` query string and percent-decode its value.
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| percent_decode(v))
    })
}

/// Percent-decode a URL component. `%XX` becomes its byte; `+` is left as-is
/// (we never encode spaces as `+`). Invalid escapes are passed through verbatim.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // `%XX` (two hex digits) decodes to one byte; anything else passes through.
        if bytes[i] == b'%' {
            if let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|b| hex_val(*b)),
                bytes.get(i + 2).and_then(|b| hex_val(*b)),
            ) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Value of a single hex digit, or `None` if it isn't one.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
