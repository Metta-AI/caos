//! Compute: the `/run` pipeline.
//!
//! A request is a content-addressed tree `{image, args, std, salt}`; `/run?req=<hash>`
//! reads it, then: cache lookup (Redis) → run-cycle detection → image resolution
//! (a `docker://` ref used as-is, or a git-docker image converted and pushed to
//! the registry) → the worker container run, whose stdout is `"<type> <hash>"`.
//! A top-level run also pins `refs/caos/res/<req>` at the result. Results,
//! converted images, and built layers are all cached in Redis (best-effort).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::storage::{fetch_blob, fetch_tree};
use crate::{Config, HttpError};

/// Repository name converted images are pushed under. They're addressed by
/// digest, so the name is arbitrary and fixed.
const REGISTRY_REPO: &str = "caos";

/// Prefix marking the `image` parameter as an ordinary docker reference rather
/// than one of our git images (the default).
const DOCKER_SCHEME: &str = "docker://";

/// Media type for the uncompressed-tar layers we build from git trees. Base
/// layers pulled from another registry keep their own (often gzipped) media type.
const OCI_LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar";

/// How long to wait on Redis before giving up and running uncached.
const REDIS_TIMEOUT: Duration = Duration::from_secs(5);

/// The caos binary inside every compute image, forced as the entrypoint.
const CAOS_BIN: &str = "/bin/caos";

/// Env var carrying the run stack — the newline-separated `(image, args)`
/// computations currently in progress. We set it on each spawned worker (this
/// computation appended); `caos run` echoes it back via the `stack` query param
/// so we can catch a run that re-enters a computation already on the stack.
/// Threaded through env, never the args tree, so it doesn't affect the cache key.
const RUN_STACK_ENV: &str = "CAOS_RUN_STACK";

/// Reserved suffix for the per-entry permission sidecars `import-image` writes.
const META_SUFFIX: &str = ".caosmeta";

/// Disambiguates temp dirs created across handler threads.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `GET /run?req=<reqHash>[&stack=...]` — run the request object `<reqHash>` (a
/// tree `{image, args, std, salt}`) and return its result as `"<type> <hash>"`.
///
/// The request being a content-addressed object means `reqHash` *is* the cache
/// key (it captures image + args + std) and the rendezvous id: a top-level run
/// also pins `refs/caos/res/<reqHash>` at the result, so a client can fetch it by
/// ref. Workers POST the request via `/object` and call this; the CLI pushes it.
pub(crate) fn run(config: &Config, query: &str) -> Result<Vec<u8>, HttpError> {
    let req = query_param(query, "req")
        .ok_or_else(|| HttpError::new(400, "missing 'req' query parameter"))?;
    if req.is_empty() || !req.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HttpError::new(400, format!("invalid req hash: {req:?}")));
    }

    // Unpack the request: image (a ref blob), args (a tree), std (a ref blob),
    // salt (an opaque blob). `std` names the standard library, materialized at
    // `/cas/std` in the worker; `salt` is a cache-buster. Both are part of the
    // request (hence the key) and threaded into the worker.
    let (image, args, std, salt) = read_request(config, &req)?;
    if image.is_empty() {
        return Err(HttpError::new(400, "request has empty image"));
    }
    // The args hash is interpolated into `--args=`; require a plain hex object id.
    if args.is_empty() || !args.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HttpError::new(400, format!("invalid args hash: {args:?}")));
    }
    if !std.is_empty() && !std.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HttpError::new(400, format!("invalid std hash: {std:?}")));
    }

    // The run stack (cycle detection) is the chain of request hashes in progress,
    // threaded through nested runs via CAOS_RUN_STACK (echoed back as `stack`). An
    // empty stack means this is a top-level (external) run — the one we pin a
    // result ref for; nested runs are transient.
    let incoming = query_param(query, "stack").unwrap_or_default();
    let stack: Vec<&str> = incoming.lines().filter(|l| !l.is_empty()).collect();
    let top_level = stack.is_empty();

    // The request hash is the cache key (it captures image+args+std); the value is
    // the result "<type> <hash>". A hit skips image conversion and the container
    // run. Redis is best-effort: a lookup error just means we run uncached.
    let key = format!("caos:result:{req}");
    match cache_get(&config.redis_addr, &key) {
        Ok(Some(result)) => {
            eprintln!("cache hit: req={req} -> {result}");
            if top_level {
                pin_result(config, &req, &result);
            }
            return Ok(format!("{result}\n").into_bytes());
        }
        Ok(None) => eprintln!("cache miss: req={req} (image={image} args={args}); running worker"),
        Err(e) => eprintln!("cache lookup failed ({e}); running worker: req={req}"),
    }

    // Re-entering a request already on the stack has no fixpoint — fail, listing
    // the cycle. (A cache hit can't be on the stack: a cyclic computation never
    // completes, so it never caches, which is why checking only on a miss is
    // sound.) The request hash is exactly this frame's identity.
    if let Some(pos) = stack.iter().position(|&f| f == req) {
        let mut cycle: Vec<&str> = stack[pos..].to_vec();
        cycle.push(&req);
        let listing = cycle.join("\n  -> ");
        eprintln!("run cycle detected:\n  {listing}");
        return Err(HttpError::new(
            400,
            format!("run cycle detected:\n  {listing}"),
        ));
    }
    // Child runs see this computation as an ancestor.
    let mut child_stack: Vec<&str> = stack.clone();
    child_stack.push(&req);
    let child_stack = child_stack.join("\n");

    // Resolve to a reference the host's docker daemon can run: a `docker://`
    // image is used directly; one of our git images is converted to a real image,
    // pushed to the registry, and referenced by digest.
    let docker_ref = resolve_image(config, &image)?;

    let output = Command::new(&config.docker_bin)
        .arg("run")
        .arg("--rm")
        .args(["--network", &config.network])
        // One server, so the worker gets one URL for `caos get`/`put`/`run`.
        .args(["-e", &format!("CAOS_SERVER_URL={}", config.server_url)])
        // The built-in tree, materialized at /cas/std and re-passed by nested
        // `caos run`s so it threads down the whole tree (empty = none).
        .args(["-e", &format!("CAOS_STD={std}")])
        // The cache-busting salt, re-passed by nested runs so the whole tree
        // shares it (empty = none).
        .args(["-e", &format!("CAOS_SALT={salt}")])
        .args(["-e", &format!("{RUN_STACK_ENV}={child_stack}")])
        .args(["--entrypoint", CAOS_BIN])
        .arg(&docker_ref)
        .arg("entrypoint")
        .arg(format!("--args={args}"))
        .output()
        .map_err(|e| HttpError::new(500, format!("running {}: {e}", config.docker_bin)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!(
            "worker failed: req={req} ({}):\n{}",
            output.status,
            stderr.trim_end()
        );
        return Err(HttpError::new(
            500,
            format!("worker container failed ({}):\n{stderr}", output.status),
        ));
    }

    // The container's stdout is "<type> <hash>" printed by `caos entrypoint`.
    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if result_hash(&result).is_empty() {
        eprintln!("worker produced no result on stdout: req={req}");
        return Err(HttpError::new(
            500,
            "worker container produced no result on stdout",
        ));
    }

    // Cache the result for next time (best-effort).
    match cache_set(&config.redis_addr, &key, &result) {
        Ok(()) => eprintln!("ran worker: req={req} -> {result} (cached)"),
        Err(e) => eprintln!("ran worker: req={req} -> {result} (cache store failed: {e})"),
    }

    // Pin a top-level (external) run's result so a client can fetch it by ref and
    // it survives gc; nested runs set no ref (they'd flood the namespace).
    if top_level {
        pin_result(config, &req, &result);
    }

    Ok(format!("{result}\n").into_bytes())
}

/// Unpack a request object (a tree `{image, args, std, salt}`) into its parts:
/// the image ref, the args-tree hash, the std-tree hash (empty if none), and the
/// salt (empty if none).
fn read_request(config: &Config, req: &str) -> Result<(String, String, String, String), HttpError> {
    let entries = fetch_tree(config, req)
        .map_err(|e| HttpError::new(400, format!("reading request: {e}")))?;
    let mut image = None;
    let mut args = None;
    let mut std = String::new();
    let mut salt = String::new();
    for entry in entries {
        match entry.name.as_str() {
            "image" => image = Some(blob_string(config, &entry.oid.to_string())?),
            "args" => args = Some(entry.oid.to_string()),
            "std" => std = blob_string(config, &entry.oid.to_string())?,
            "salt" => salt = blob_string(config, &entry.oid.to_string())?,
            _ => {}
        }
    }
    let image = image.ok_or_else(|| HttpError::new(400, "request missing 'image'"))?;
    let args = args.ok_or_else(|| HttpError::new(400, "request missing 'args'"))?;
    Ok((image, args, std, salt))
}

/// Fetch a blob and return its content as a trimmed string.
fn blob_string(config: &Config, hash: &str) -> Result<String, HttpError> {
    let bytes =
        fetch_blob(config, hash).map_err(|e| HttpError::new(400, format!("reading blob: {e}")))?;
    String::from_utf8(bytes)
        .map(|s| s.trim().to_string())
        .map_err(|e| HttpError::new(400, format!("blob {hash} not UTF-8: {e}")))
}

/// The hash in a `"<type> <hash>"` result string (empty if malformed).
fn result_hash(result: &str) -> &str {
    result.split_whitespace().nth(1).unwrap_or("")
}

/// Pin `refs/caos/res/<req>` at the result so a client can fetch it by ref and it
/// survives gc. Best-effort: a failure just means the result isn't ref-pinned
/// (it's still cached and reachable by hash). `result` is `"<type> <hash>"`.
fn pin_result(config: &Config, req: &str, result: &str) {
    let hash = result_hash(result);
    if hash.is_empty() {
        return;
    }
    let refname = format!("refs/caos/res/{req}");
    match Command::new("git")
        .args(["-C", &config.git_dir, "update-ref", &refname, hash])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("warning: git update-ref {refname} exited with {status}"),
        Err(e) => eprintln!("warning: pinning {refname}: {e}"),
    }
}

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

/// Convert the git-docker image tree `git_hash` to a real image and push it to
/// the registry, returning a digest reference. Cached in Redis by git hash.
fn convert_git_image(config: &Config, git_hash: &str) -> Result<String, String> {
    let image_key = format!("caos:image:{git_hash}");
    if let Ok(Some(manifest_digest)) = cache_get(&config.redis_addr, &image_key) {
        eprintln!("image cache hit: {git_hash} -> {manifest_digest}");
        return Ok(image_ref(config, &manifest_digest));
    }

    // The image tree holds `config.json` (a blob), `layer<NN>` subtrees, and an
    // optional `base` blob naming a `docker://<ref>` to stack our layers on top
    // of — so a heavy toolchain rides as registry layers pulled from its source,
    // never as git objects.
    let mut config_oid: Option<String> = None;
    let mut base_oid: Option<String> = None;
    let mut layers: Vec<(u64, String)> = Vec::new();
    for entry in fetch_tree(config, git_hash)? {
        if entry.name == "config.json" {
            config_oid = Some(entry.oid.to_string());
        } else if entry.name == "base" {
            base_oid = Some(entry.oid.to_string());
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
    let has_base = base_oid.is_some();
    if !has_base && layers.is_empty() {
        return Err("image tree has no base and no layer<NN> entries".to_string());
    }
    layers.sort_by_key(|(num, _)| *num);

    // A manifest layer is (mediaType, digest, size); a diff_id is the layer's
    // *uncompressed* sha256. A `base`'s layers and diff_ids come from the copied
    // base image (its layers are usually gzipped, so digest != diff_id). Our own
    // layers are uncompressed tar, so digest == diff_id. Base layers go on the
    // bottom; ours stack on top.
    let mut manifest_layers: Vec<(String, String, u64)> = Vec::new();
    let mut diff_ids: Vec<String> = Vec::new();
    if let Some(base_oid) = base_oid {
        let base_ref = String::from_utf8(fetch_blob(config, &base_oid)?)
            .map_err(|e| format!("base ref not UTF-8: {e}"))?;
        let base_ref = base_ref.trim();
        let base_ref = base_ref.strip_prefix(DOCKER_SCHEME).unwrap_or(base_ref);
        if base_ref.is_empty() {
            return Err("base blob is empty".to_string());
        }
        let (base_layers, base_diff_ids) = fetch_base(config, base_ref)?;
        diff_ids.extend(base_diff_ids);
        manifest_layers.extend(base_layers);
    }
    for (_, oid) in &layers {
        let (digest, size) = ensure_layer(config, oid)?;
        diff_ids.push(digest.clone());
        manifest_layers.push((OCI_LAYER_MEDIA_TYPE.to_string(), digest, size));
    }

    // Set the config's diff_ids to the full stack (base ++ ours) so the image is
    // self-consistent. We generate them outright — the stored config needn't
    // carry diff_ids (the producer can't know them without tarring / resolving
    // the base).
    let config_bytes = fetch_blob(config, &config_oid)?;
    let new_config = set_config_diff_ids(&config_bytes, &diff_ids)?;
    let config_digest = format!("sha256:{}", sha256_hex(&new_config));
    push_blob(config, &config_digest, &new_config)?;

    let manifest = build_manifest(&config_digest, new_config.len() as u64, &manifest_layers);
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

/// Copy a base image (`base_ref`, a bare docker reference) from its source
/// registry into our own repo with skopeo, so its blobs are available for a
/// converted git image to reference. Returns the base's manifest layers
/// `(media_type, digest, size)` and its config `diff_id`s (uncompressed digests)
/// — the lower part of the stack our delta layers sit on. `--format oci` rewrites
/// the manifest to OCI media types so it composes cleanly with our OCI layers;
/// the layer *blobs* (and their digests) are untouched.
fn fetch_base(config: &Config, base_ref: &str) -> Result<(Vec<(String, String, u64)>, Vec<String>), String> {
    let push = config.registry_push_url.trim_end_matches('/');
    let host = push
        .strip_prefix("http://")
        .or_else(|| push.strip_prefix("https://"))
        .unwrap_or(push);
    // A deterministic tag per base ref: re-converting reuses the same copy.
    let tag = format!("base-{}", sha256_hex(base_ref.as_bytes()));
    let dest = format!("docker://{host}/{REGISTRY_REPO}:{tag}");
    let status = Command::new("skopeo")
        .args([
            "--insecure-policy",
            "copy",
            "--format",
            "oci",
            "--dest-tls-verify=false",
            "--override-os",
            "linux",
            "--override-arch",
            "amd64",
        ])
        .arg(format!("docker://{base_ref}"))
        .arg(&dest)
        // The slim server image runs as uid 0 with no /etc/passwd entry, so
        // skopeo can't resolve $HOME (it wants one for its auth/config dirs).
        // Point it at a writable dir so the anonymous pull works.
        .env("HOME", "/tmp")
        .status()
        .map_err(|e| format!("skopeo copy {base_ref}: {e}"))?;
    if !status.success() {
        return Err(format!("skopeo copy {base_ref} -> {dest} failed ({status})"));
    }

    // Read the manifest we just wrote: the base layers' media types/digests/sizes.
    let man_url = format!("{push}/v2/{REGISTRY_REPO}/manifests/{tag}");
    let resp = minreq::get(&man_url)
        .with_header(
            "Accept",
            "application/vnd.oci.image.manifest.v1+json, \
             application/vnd.docker.distribution.manifest.v2+json",
        )
        .send()
        .map_err(|e| format!("GET {man_url}: {e}"))?;
    if !(200..300).contains(&resp.status_code) {
        return Err(format!(
            "reading base manifest {tag}: {} {}",
            resp.status_code, resp.reason_phrase
        ));
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(resp.as_bytes()).map_err(|e| format!("parsing base manifest: {e}"))?;
    let layers = manifest["layers"]
        .as_array()
        .ok_or("base manifest has no layers")?
        .iter()
        .map(|l| {
            let media = l["mediaType"]
                .as_str()
                .unwrap_or(OCI_LAYER_MEDIA_TYPE)
                .to_string();
            let digest = l["digest"].as_str().unwrap_or_default().to_string();
            let size = l["size"].as_u64().unwrap_or_default();
            (media, digest, size)
        })
        .collect::<Vec<_>>();
    let config_digest = manifest["config"]["digest"]
        .as_str()
        .ok_or("base manifest has no config digest")?;

    // Read the base config blob for its uncompressed diff_ids.
    let cfg_url = format!("{push}/v2/{REGISTRY_REPO}/blobs/{config_digest}");
    let resp = minreq::get(&cfg_url)
        .send()
        .map_err(|e| format!("GET {cfg_url}: {e}"))?;
    if !(200..300).contains(&resp.status_code) {
        return Err(format!(
            "reading base config {config_digest}: {} {}",
            resp.status_code, resp.reason_phrase
        ));
    }
    let cfg: serde_json::Value =
        serde_json::from_slice(resp.as_bytes()).map_err(|e| format!("parsing base config: {e}"))?;
    let diff_ids = cfg["rootfs"]["diff_ids"]
        .as_array()
        .ok_or("base config has no rootfs.diff_ids")?
        .iter()
        .map(|d| d.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    if layers.len() != diff_ids.len() {
        return Err(format!(
            "base layer/diff_id count mismatch: {} layers vs {} diff_ids",
            layers.len(),
            diff_ids.len()
        ));
    }
    Ok((layers, diff_ids))
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

/// Materialize a layer's git tree to a temp dir, apply its `.caosmeta` sidecars,
/// and tar it deterministically (GNU format handles the long /nix/store paths and
/// symlinks; the flags zero the mtimes and sort entries, so the output — hence its
/// digest — is stable).
fn build_layer_tar(config: &Config, tree_hash: &str) -> Result<Vec<u8>, String> {
    let dir = temp_dir()?;
    let result = (|| {
        materialize_tree(config, &dir, tree_hash)?;
        apply_layer_metadata(&dir)?;
        tar_dir(&dir)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Apply the `<name>.caosmeta` sidecars written by `import-image`: for each one,
/// restore the sibling entry's mode and owner, then remove the sidecar so it
/// doesn't land in the layer tar. We run as root, so chmod/chown/unlink and the
/// later read-for-tar all work regardless of the perms we set.
fn apply_layer_metadata(dir: &Path) -> Result<(), String> {
    let mut sidecars = Vec::new();
    let mut subdirs = Vec::new();
    for dirent in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let dirent = dirent.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = dirent.file_name().to_string_lossy().into_owned();
        if let Some(target) = name.strip_suffix(META_SUFFIX) {
            sidecars.push((dirent.path(), dir.join(target)));
        } else if dirent
            .file_type()
            .map_err(|e| format!("{}: {e}", dirent.path().display()))?
            .is_dir()
        {
            subdirs.push(dirent.path());
        }
    }

    for (sidecar, target) in sidecars {
        let bytes = std::fs::read(&sidecar).map_err(|e| format!("{}: {e}", sidecar.display()))?;
        let meta: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", sidecar.display()))?;
        let mode = meta
            .get("mode")
            .and_then(|v| v.as_str())
            .and_then(|s| u32::from_str_radix(s, 8).ok())
            .ok_or_else(|| format!("{}: missing/invalid mode", sidecar.display()))?;
        let uid = meta.get("uid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let gid = meta.get("gid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        std::os::unix::fs::chown(&target, Some(uid), Some(gid))
            .map_err(|e| format!("chown {}: {e}", target.display()))?;
        set_mode(&target, mode)?;
        std::fs::remove_file(&sidecar).map_err(|e| format!("{}: {e}", sidecar.display()))?;
    }

    for subdir in subdirs {
        apply_layer_metadata(&subdir)?;
    }
    Ok(())
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

/// Tar `dir`'s contents reproducibly (GNU format, zeroed mtimes, sorted, numeric
/// owners read from disk — which the `.caosmeta` sidecars already set).
fn tar_dir(dir: &Path) -> Result<Vec<u8>, String> {
    let output = Command::new("tar")
        .args([
            "--format=gnu",
            "--numeric-owner",
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
    layers: &[(String, String, u64)],
) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_size,
        },
        "layers": layers.iter().map(|(media_type, digest, size)| serde_json::json!({
            "mediaType": media_type,
            "digest": digest,
            "size": size,
        })).collect::<Vec<_>>(),
    })
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
