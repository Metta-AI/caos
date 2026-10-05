//! The client's side of ref writers (design/ref-writers.md): the writer key it
//! signs with, the commands that manage keys, namespaces and their writers, and
//! the namespaces its conversations live in.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use caos::GitTransport;
use conversation_protocol::v3::writers::{self, Writer, WriterKey};
use conversation_protocol::v3::{refs, GitStore, Oid, RefUpdate};

/// The checkout's git config entry holding this client's private writer key.
pub const KEY_CONFIG: &str = "caos.ref-writer-key";

static KEY: OnceLock<Option<WriterKey>> = OnceLock::new();

/// This process's writer key: `caos.ref-writer-key`, read once.
pub fn key() -> Result<Option<&'static WriterKey>, String> {
    #[cfg(test)]
    set_key(WriterKey::from_seed([7; 32]));
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

/// Use `key` instead of the checkout's config, for a process that is handed
/// one (tests). Only the first call takes effect.
pub fn set_key(key: WriterKey) {
    let _ = KEY.set(Some(key));
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

/// `writers list|add|remove <namespace|conversation> [<key> [<label>]]`.
pub fn cli_writers(t: &GitTransport, args: &[String]) -> Result<(), String> {
    let usage =
        || "usage: writers list|add|remove <namespace|conversation> [<key> [<label>]]".to_string();
    let (command, target, rest) = match args {
        [command, target, rest @ ..] => (command.as_str(), target.as_str(), rest),
        _ => return Err(usage()),
    };
    let namespace = if writers::is_namespace(target) {
        target.to_string()
    } else {
        let id = resolve(t, target)?;
        namespace_of(t, &id)?
    };
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

// ---- conversations --------------------------------------------------------------

/// The namespace holding this writer's conversation memberships.
pub fn personal_namespace() -> Result<String, String> {
    Ok(refs::personal_namespace(&require_key()?.public()))
}

/// The namespace a new conversation `id` of this writer's is created in.
pub fn own_namespace(id: &str) -> Result<String, String> {
    refs::conversation_namespace(&require_key()?.public(), id)
}

static NAMESPACES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn known() -> std::sync::MutexGuard<'static, HashMap<String, String>> {
    NAMESPACES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Remember that conversation `id` lives in `namespace`, e.g. a child found
/// through its parent.
pub fn remember(id: &str, namespace: &str) {
    known().insert(id.to_string(), namespace.to_string());
}

/// Which conversation a user means: an address `<namespace>/<id>`, or a bare
/// id resolved by [`namespace_of`]. Returns the id, its namespace remembered.
pub fn resolve(t: &GitTransport, name: &str) -> Result<String, String> {
    if let Ok((namespace, id)) = refs::parse_address(name) {
        remember(&id, &namespace);
        return Ok(id);
    }
    namespace_of(t, name)?;
    Ok(name.to_string())
}

/// The namespace conversation `id` lives in. This writer's own comes first;
/// otherwise the one namespace holding a conversation of that id. A new id is
/// this writer's.
pub fn namespace_of(t: &GitTransport, id: &str) -> Result<String, String> {
    if let Some(namespace) = known().get(id) {
        return Ok(namespace.clone());
    }
    let mine = key()?
        .map(|key| refs::conversation_namespace(&key.public(), id))
        .transpose()?;
    let pattern = refs::head_ref_pattern(id)?;
    let listing = t.git_capture(&["ls-remote", "--refs", caos::CAOS_REMOTE, &pattern], None)?;
    let mut found: Vec<String> = listing
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter_map(|(_, refname)| refs::parse_head_ref(refname).ok())
        .filter(|(_, found)| found == id)
        .map(|(namespace, _)| namespace)
        .collect();
    found.sort();
    found.dedup();
    let namespace = match (mine, found.as_slice()) {
        (Some(mine), found) if found.contains(&mine) => mine,
        (_, [only]) => only.clone(),
        (Some(mine), []) => mine,
        (None, []) => return Err(missing_key()),
        (_, many) => {
            return Err(format!(
                "conversation {id:?} exists in several namespaces; name one by its address: {}",
                many.iter()
                    .map(|ns| refs::address(ns, id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    };
    remember(id, &namespace);
    Ok(namespace)
}

/// The head ref of conversation `id`.
pub fn head_ref(t: &GitTransport, id: &str) -> Result<String, String> {
    refs::head_ref(&namespace_of(t, id)?, id)
}

/// The address of conversation `id`, `<namespace>/<id>`.
pub fn address(t: &GitTransport, id: &str) -> Result<String, String> {
    Ok(refs::address(&namespace_of(t, id)?, id))
}

/// Create conversation `id`'s `namespace` on its own, ahead of its first head
/// (a namespace with nothing in it yet is harmless).
pub fn ensure_namespace(store: &mut GitStore, namespace: &str, id: &str) -> Result<(), String> {
    let updates = create_namespace_update(store, namespace, &conversation_label(id))?;
    store.push(&updates)
}

/// The label a conversation's namespace was created with; see
/// [`refs::conversation_namespace`].
pub fn conversation_label(id: &str) -> String {
    format!("conversation {}", refs::key_of(id))
}

pub const PERSONAL_LABEL: &str = "personal";

/// `conversation-ref <id|address>`: the conversation's head ref.
pub fn cli_conversation_ref(t: &GitTransport, args: &[String]) -> Result<(), String> {
    let [name] = args else {
        return Err("usage: conversation-ref <id|namespace/id>".to_string());
    };
    let id = resolve(t, name)?;
    println!("{}", head_ref(t, &id)?);
    Ok(())
}
