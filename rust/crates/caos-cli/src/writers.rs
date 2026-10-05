//! The client's side of ref writers (design/ref-writers.md): the writer key it
//! signs with, and the commands that manage keys, namespaces and their writers.

use std::sync::OnceLock;

use caos::GitTransport;
use conversation_protocol::v3::writers::{self, Writer, WriterKey};
use conversation_protocol::v3::{GitStore, Oid, RefUpdate};

/// The checkout's git config entry holding this client's private writer key.
pub const KEY_CONFIG: &str = "caos.ref-writer-key";

static KEY: OnceLock<Option<WriterKey>> = OnceLock::new();

/// This process's writer key: `caos.ref-writer-key`, read once.
pub fn key() -> Result<Option<&'static WriterKey>, String> {
    if KEY.get().is_none() {
        let configured = std::process::Command::new("git")
            .args(["config", "--get", KEY_CONFIG])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|value| !value.is_empty());
        let key = configured
            .map(|seed| WriterKey::parse(&seed).map_err(|e| format!("{KEY_CONFIG}: {e}")))
            .transpose()?;
        let _ = KEY.set(key);
    }
    Ok(KEY.get().and_then(Option::as_ref))
}

pub fn require_key() -> Result<&'static WriterKey, String> {
    key()?.ok_or_else(missing_key)
}

fn missing_key() -> String {
    format!(
        "this needs a ref writer key: run `caos-cli ref-writer-key new` and set it with \
         `git config {KEY_CONFIG} <key>` (design/ref-writers.md)"
    )
}

/// Sign this process's compute requests with its writer key, so the jobs they
/// start may write what they ask for. A process with no key signs nothing.
pub fn install_request_signer() {
    if let Ok(Some(key)) = key() {
        caos::set_request_signer(Box::new(move |method: &str, target: &str| {
            (writers::ADMIT_HEADER, key.sign_admission(method, target))
        }));
    }
}

/// Sign `store`'s pushes with this process's writer key, if it has one.
pub fn sign_pushes(store: &mut GitStore) -> Result<(), String> {
    if let Some(key) = key()? {
        store.set_push_auth(Box::new(move |commands: &[writers::Command]| {
            key.sign_update(commands)
        }));
    }
    Ok(())
}

/// The update creating `namespace` with this writer alone, if the server does
/// not hold it yet; push it together with the namespace's first ref.
pub fn create_namespace_update(
    store: &mut GitStore,
    namespace: &str,
    label: &str,
) -> Result<Vec<RefUpdate>, String> {
    let writers_ref = writers::writers_ref(namespace);
    if store.read_ref(&writers_ref)?.is_some() {
        return Ok(Vec::new());
    }
    let genesis = writers::genesis_commit(
        store,
        &writers::sole_writer(&require_key()?.public()),
        label,
    )?;
    if genesis.as_str() != namespace {
        return Err(format!(
            "namespace {namespace} is not this writer's to create (it is not the hash of a list \
             holding only this key)"
        ));
    }
    Ok(vec![RefUpdate {
        refname: writers_ref,
        expected: None,
        new: Some(genesis),
    }])
}

// ---- commands ---------------------------------------------------------------------

/// `ref-writer-key new|show`.
pub fn cli_key(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("new") => {
            let mut seed = [0u8; 32];
            use std::io::Read;
            std::fs::File::open("/dev/urandom")
                .and_then(|mut f| f.read_exact(&mut seed))
                .map_err(|e| format!("reading /dev/urandom: {e}"))?;
            let key = WriterKey::from_seed(seed);
            eprintln!(
                "public key: {}\nset the private key (stdout) with `git config {KEY_CONFIG} <key>`",
                key.public()
            );
            println!("{}", key.seed_hex());
            Ok(())
        }
        Some("show") => {
            println!("{}", require_key()?.public());
            Ok(())
        }
        _ => Err("usage: ref-writer-key new|show".to_string()),
    }
}

/// `namespace new [<label>]`: create a namespace with this writer alone, and
/// print its id.
pub fn cli_namespace(t: &GitTransport, args: &[String]) -> Result<(), String> {
    let label = match args {
        [command] if command == "new" => "namespace".to_string(),
        [command, label] if command == "new" => label.clone(),
        _ => return Err("usage: namespace new [<label>]".to_string()),
    };
    let mut store = crate::open_store(t)?;
    let namespace = writers::genesis_id(&writers::sole_writer(&require_key()?.public()), &label);
    let updates = create_namespace_update(&mut store, namespace.as_str(), &label)?;
    store.push(&updates)?;
    println!("{namespace}");
    Ok(())
}

/// `writers list|add|remove <namespace> [<key> [<label>]]`.
pub fn cli_writers(t: &GitTransport, args: &[String]) -> Result<(), String> {
    let usage = || "usage: writers list|add|remove <namespace> [<key> [<label>]]".to_string();
    let (command, target, rest) = match args {
        [command, target, rest @ ..] => (command.as_str(), target.as_str(), rest),
        _ => return Err(usage()),
    };
    writers::validate_namespace(target)?;
    let namespace = target.to_string();
    match (command, rest) {
        ("list", []) => {
            for writer in list(t, &namespace)?.1 {
                println!("{}  {}", writer.key, writer.label);
            }
            Ok(())
        }
        ("add", [key, label @ ..]) if label.len() <= 1 => {
            if !add(t, &namespace, key, label.first().map_or("", String::as_str))? {
                eprintln!("{key} already writes {namespace}");
            }
            Ok(())
        }
        ("remove", [key]) => change(t, &namespace, |list| {
            let before = list.len();
            list.retain(|w| &w.key != key);
            if list.len() == before {
                return Err(format!("{key} does not write {namespace}"));
            }
            Ok(format!("remove writer {key}"))
        }),
        _ => Err(usage()),
    }
}

/// Add `key` to `namespace`'s writers; false if it already was one.
pub fn add(t: &GitTransport, namespace: &str, key: &str, label: &str) -> Result<bool, String> {
    if !writers::is_key(key) {
        return Err(format!("{key:?} is not an ed25519 public key"));
    }
    if list(t, namespace)?.1.iter().any(|w| w.key == key) {
        return Ok(false);
    }
    change(t, namespace, |list| {
        list.push(Writer {
            key: key.to_string(),
            label: label.to_string(),
        });
        Ok(format!("add writer {key}"))
    })?;
    Ok(true)
}

/// The current `writers` commit of `namespace` and its list.
pub fn list(t: &GitTransport, namespace: &str) -> Result<(Oid, Vec<Writer>), String> {
    let store = crate::open_store(t)?;
    let tip = store
        .fetch_ref(&writers::writers_ref(namespace))?
        .ok_or_else(|| format!("namespace {namespace} has no writers ref"))?;
    let writers = writers::read_writers(&store, &tip)?;
    Ok((tip, writers))
}

fn change(
    t: &GitTransport,
    namespace: &str,
    edit: impl Fn(&mut Vec<Writer>) -> Result<String, String>,
) -> Result<(), String> {
    let (tip, mut list) = self::list(t, namespace)?;
    let message = edit(&mut list)?;
    let mut store = crate::open_store(t)?;
    let author = crate::signature("writers")?;
    let next = writers::writers_commit(&mut store, &tip, &list, &author, &message)?;
    store.push(&[RefUpdate {
        refname: writers::writers_ref(namespace),
        expected: Some(tip),
        new: Some(next),
    }])
}

/// `ref-push <rev> <ref>`: point a governed ref at a local commit, signed.
pub fn cli_ref_push(t: &GitTransport, args: &[String]) -> Result<(), String> {
    let [rev, refname] = args else {
        return Err("usage: ref-push <rev> <ref>".to_string());
    };
    let commit = t
        .git_capture(
            &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
            None,
        )?
        .trim()
        .to_string();
    let commit = Oid::parse(&commit, "commit")?;
    let store = crate::open_store(t)?;
    let current = store.read_ref(refname)?;
    store.push(&[RefUpdate {
        refname: refname.clone(),
        expected: current,
        new: Some(commit),
    }])
}
