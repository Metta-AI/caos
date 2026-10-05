//! Who may write which ref (design/ref-writers.md).
//!
//! Three parts, one per moment a write is decided:
//! * admission: a top-level request's `X-Caos-Write` header names the writer
//!   it acts for ([`admit`]);
//! * dispatch: a job asking for namespaces in its `writes` arg gets a run token
//!   covering what it asked for and its creator held, and its children inherit
//!   only that ([`grant`]);
//! * the push: the repository's pre-receive hook re-execs this binary
//!   ([`pre_receive`]) and checks every command against the namespace's
//!   `writers` list.

use std::collections::HashMap;
use std::io::Read;
use std::process::Command;
use std::sync::{Arc, Mutex};

use conversation_protocol::v3::writers::{
    self, parse_writers, split_ref, update_message, Auth, Command as RefCommand, Writer,
    WRITERS_PATH, WRITERS_REF, ZERO_OID,
};

use crate::{Config, HttpError};

/// The argv that makes this binary the pre-receive hook.
pub(crate) const HOOK_ARG: &str = "--pre-receive";
/// Where the hook asks for a run token's grant.
pub(crate) const TOKEN_PATH: &str = "/ref-writers/token";
const SERVER_ENV: &str = "CAOS_REF_WRITERS_SERVER";
const HOOK_MARKER: &str = "# managed by caos-server: ref writers";
/// Hooks older servers installed; ours replaces them.
const OLD_HOOK_MARKERS: [&str; 2] = [
    "# managed by caos-server: append-only refs",
    "# managed by caos-server: append-only conversation heads",
];
/// Refs named by their own content: each must point at its last component.
const CONTENT_NAMED: [&str; 2] = ["refs/caos/req/", "refs/heads/caos-test/"];
/// `report` logs what enforcement would refuse and accepts it, for rollout.
const MODE_CONFIG: &str = "caos.refWriters";
/// Refs any push may write. Each is a hole: `caosd up`'s dev publish is the
/// one the server keeps open by default.
pub(crate) const UNGUARDED_CONFIG: &str = "caos.unguardedRef";
pub(crate) const DEFAULT_UNGUARDED: &str = "refs/caos/dev";

/// Which writer a run acts for, and the namespaces it may still hand on.
#[derive(Clone, Default)]
pub(crate) enum Writes {
    #[default]
    None,
    As {
        key: String,
        /// `None`: whatever the writer may write (a top-level request).
        scope: Option<Arc<Vec<String>>>,
    },
}

/// A top-level request's authority, from its `X-Caos-Write` header.
pub(crate) fn admit(header: Option<&str>, method: &str, target: &str) -> Result<Writes, HttpError> {
    let Some(header) = header else {
        return Ok(Writes::None);
    };
    let refuse = |m: String| HttpError::new(403, format!("{}: {m}", writers::ADMIT_HEADER));
    let (key, expiry, signature) = writers::parse_admit_header(header).map_err(refuse)?;
    if expiry < writers::now() {
        return Err(refuse("expired".to_string()));
    }
    writers::verify(
        &key,
        &writers::admit_message(expiry, method, target),
        &signature,
    )
    .map_err(refuse)?;
    Ok(Writes::As { key, scope: None })
}

// ---- run tokens ---------------------------------------------------------------

#[derive(Clone)]
struct Grant {
    key: String,
    /// `None`: every namespace the writer may write.
    namespaces: Option<Vec<String>>,
}

static TOKENS: Mutex<Option<HashMap<String, Grant>>> = Mutex::new(None);

/// A dispatched job's write grant: what it asked for in `writes`, within what
/// it was handed. Returns the token to inject (revoke it when the job's
/// container is done) and what the job's children may in turn be handed.
pub(crate) fn grant(
    config: &Config,
    writes: &Writes,
    arg_entries: &std::collections::BTreeMap<String, String>,
) -> Result<(Option<String>, Writes), HttpError> {
    let Writes::As { key, scope } = writes else {
        return Ok((None, Writes::None));
    };
    let Some(oid) = arg_entries.get(writers::WRITES_ARG) else {
        return Ok((None, Writes::None));
    };
    let asked = crate::storage::fetch_blob(config, oid)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(|| HttpError::new(400, "the `writes` arg must be a text blob"))?;
    let granted: Option<Vec<String>> =
        match writers::parse_writes(&asked).map_err(|e| HttpError::new(400, e))? {
            writers::Writes::All => scope.as_ref().map(|scope| scope.to_vec()),
            writers::Writes::Namespaces(asked) => Some(
                asked
                    .into_iter()
                    .filter(|ns| scope.as_ref().is_none_or(|scope| scope.contains(ns)))
                    .collect(),
            ),
        };
    if granted.as_ref().is_some_and(Vec::is_empty) {
        return Ok((None, Writes::None));
    }
    let token = random_token()?;
    TOKENS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(
            token.clone(),
            Grant {
                key: key.clone(),
                namespaces: granted.clone(),
            },
        );
    Ok((
        Some(token),
        Writes::As {
            key: key.clone(),
            scope: granted.map(Arc::new),
        },
    ))
}

pub(crate) fn revoke(token: &str) {
    if let Some(tokens) = TOKENS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        tokens.remove(token);
    }
}

/// `POST /ref-writers/token`: what a run token may write, for the hook. It
/// tells a caller nothing it could use: holding the token is already holding
/// the grant.
pub(crate) fn token_endpoint(body: &str) -> Result<Vec<u8>, HttpError> {
    let grant = TOKENS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|tokens| tokens.get(body.trim()).cloned())
        .ok_or_else(|| HttpError::new(404, "no such run token (its run has ended)"))?;
    let all = grant.namespaces.is_none();
    Ok(serde_json::json!({
        "key": grant.key,
        "namespaces": grant.namespaces.unwrap_or_default(),
        "all": all,
    })
    .to_string()
    .into_bytes())
}

fn random_token() -> Result<String, HttpError> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| HttpError::new(500, format!("reading /dev/urandom: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

// ---- installing the hook --------------------------------------------------------

/// Point the repository's pre-receive hook at this binary. Rewritten on every
/// boot so it always names the binary that is running; an administrator's own
/// hook (no marker) is refused rather than overwritten.
pub(crate) fn install_hook(git_dir: &str, server_addr: &str) -> Result<(), String> {
    let hooks = std::path::Path::new(git_dir).join("hooks");
    let hook = hooks.join("pre-receive");
    match std::fs::read_to_string(&hook) {
        Ok(existing)
            if !existing.contains(HOOK_MARKER)
                && !OLD_HOOK_MARKERS.iter().any(|m| existing.contains(m)) =>
        {
            return Err(format!(
                "{} exists and is not caos's; ref writers cannot be enforced",
                hook.display()
            ));
        }
        _ => {}
    }
    let exe = std::env::current_exe().map_err(|e| format!("locating the server binary: {e}"))?;
    let shell = find_on_path("sh")
        .or_else(|| find_on_path("bash"))
        .ok_or_else(|| "no sh or bash on PATH for the pre-receive hook".to_string())?;
    let script = format!(
        "#!{}\n{HOOK_MARKER}\n{SERVER_ENV}={} exec {} {HOOK_ARG}\n",
        shell.display(),
        shell_quote(&hook_server_url(server_addr)),
        shell_quote(&exe.display().to_string()),
    );
    std::fs::create_dir_all(&hooks).map_err(|e| format!("creating {}: {e}", hooks.display()))?;
    let temp = hooks.join(format!("pre-receive.{}", std::process::id()));
    std::fs::write(&temp, script).map_err(|e| format!("writing {}: {e}", temp.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", temp.display()))?;
    std::fs::rename(&temp, &hook).map_err(|e| format!("installing {}: {e}", hook.display()))?;
    Ok(())
}

/// The address the hook reaches this server at: the listen address, with an
/// unspecified host made loopback (the hook runs beside the server).
fn hook_server_url(server_addr: &str) -> String {
    let (host, port) = server_addr.rsplit_once(':').unwrap_or((server_addr, "80"));
    let host = match host {
        "" | "0.0.0.0" | "[::]" | "::" => "127.0.0.1",
        host => host,
    };
    format!("http://{host}:{port}")
}

fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|dir| std::path::Path::new(dir).join(name))
        .find(|path| path.is_file())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

// ---- the hook --------------------------------------------------------------------

/// Run as the repository's pre-receive hook: refuse the whole push unless every
/// command is allowed. Returns the process exit code.
pub(crate) fn pre_receive() -> i32 {
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("caos: reading pushed refs: {e}");
        return 1;
    }
    let commands: Vec<RefCommand> = input
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(RefCommand {
                old: fields.next()?.to_string(),
                new: fields.next()?.to_string(),
                refname: fields.next()?.to_string(),
            })
        })
        .collect();
    let options: Vec<String> = (0..std::env::var("GIT_PUSH_OPTION_COUNT")
        .ok()
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(0))
        .filter_map(|i| std::env::var(format!("GIT_PUSH_OPTION_{i}")).ok())
        .collect();
    let report = git(&["config", "--get", MODE_CONFIG]).is_ok_and(|mode| mode.trim() == "report");
    match check(&Repo, &commands, &options, &TokenServer) {
        Ok(()) => 0,
        Err(message) if report => {
            eprintln!("caos: ref writers would refuse this push: {message} (report only)");
            0
        }
        Err(message) => {
            eprintln!("caos: {message}");
            1
        }
    }
}

/// What the check reads from the repository. A trait so it can be tested
/// without a git process.
trait Store {
    fn ref_value(&self, refname: &str) -> Option<String>;
    /// `.caos/writers` of a commit, even one only in the push's quarantine.
    fn writers_at(&self, commit: &str) -> Result<Vec<Writer>, String>;
    fn parents(&self, commit: &str) -> Result<Vec<String>, String>;
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool;
    fn unguarded(&self) -> Vec<String>;
}

trait Tokens {
    fn lookup(&self, token: &str) -> Result<(String, Scope), String>;
}

/// Who signed or ran the push.
struct Pusher {
    key: String,
    /// A run token's grant; `None` for a writer's own signature.
    token: Option<Scope>,
}

/// What a run token may write: every namespace its writer may, or these.
#[derive(Clone)]
enum Scope {
    All,
    Only(Vec<String>),
}

impl Scope {
    fn covers(&self, namespace: &str) -> bool {
        match self {
            Scope::All => true,
            Scope::Only(namespaces) => namespaces.iter().any(|n| n == namespace),
        }
    }
}

fn check(
    store: &dyn Store,
    commands: &[RefCommand],
    options: &[String],
    tokens: &dyn Tokens,
) -> Result<(), String> {
    // Identified once, and only if the push touches a governed ref.
    let mut identified: Option<Result<Pusher, String>> = None;
    let mut pusher_of = || -> Result<Pusher, String> {
        identified
            .get_or_insert_with(|| identify(commands, options, tokens))
            .as_ref()
            .map(|p| Pusher {
                key: p.key.clone(),
                token: p.token.clone(),
            })
            .map_err(Clone::clone)
    };
    let unguarded = store.unguarded();
    // Writers lists this push establishes, so a namespace created here governs
    // the other refs created alongside it.
    let mut created: HashMap<String, Vec<Writer>> = HashMap::new();
    let mut ordered: Vec<&RefCommand> = commands.iter().collect();
    // `writers` refs first: the rest of the push is judged by them.
    ordered.sort_by_key(|c| !split_ref(&c.refname).is_some_and(|(_, rest)| rest == WRITERS_REF));
    for command in ordered {
        let refname = command.refname.as_str();
        let deny = |why: &str| Err(format!("{refname}: {why}"));
        if unguarded.iter().any(|r| r == refname) {
            continue;
        }
        if let Some(name) = CONTENT_NAMED.iter().find_map(|p| refname.strip_prefix(p)) {
            // Deleting one only unpins it; the object stays.
            if command.new == name || command.new == ZERO_OID {
                continue;
            }
            return deny("a content-named ref must point at the object it names");
        }
        let Some((namespace, rest)) = split_ref(refname) else {
            return deny(
                "not a ref this server lets anyone push; governed refs live under \
                 refs/caos/w/<namespace>/ (design/ref-writers.md)",
            );
        };
        let pusher = pusher_of().map_err(|e| format!("{refname}: {e}"))?;
        if rest == WRITERS_REF {
            // A job may found a namespace for its writer (a test harness
            // creating a conversation), but only a writer changes who writes.
            let creating = command.old == ZERO_OID;
            if let Some(scope) = &pusher.token {
                if !creating {
                    return deny(
                        "only a writer key can change who writes; a job's run token cannot",
                    );
                }
                if !scope.covers(namespace) {
                    return deny("this job's run token was not granted that namespace");
                }
            }
            if command.new == ZERO_OID {
                return deny("a namespace's writers list cannot be deleted");
            }
            let list = if command.old == ZERO_OID {
                if command.new != namespace {
                    return deny("a namespace's id is the hash of its first writers commit");
                }
                if !store.parents(&command.new)?.is_empty() {
                    return deny("a namespace's first writers commit has no parents");
                }
                store.writers_at(&command.new)?
            } else {
                let current = store.writers_at(&command.old)?;
                if !current.iter().any(|w| w.key == pusher.key) {
                    return deny(&format!("{} is not one of its writers", pusher.key));
                }
                if !store.is_ancestor(&command.old, &command.new) {
                    return deny("the writers list only moves forward (fast-forward)");
                }
                store.writers_at(&command.new)?
            };
            if command.old == ZERO_OID && !list.iter().any(|w| w.key == pusher.key) {
                return deny("the creator must be one of the first writers");
            }
            created.insert(namespace.to_string(), list);
            continue;
        }
        let list = match created.get(namespace) {
            Some(list) => list.clone(),
            None => match store.ref_value(&writers::writers_ref(namespace)) {
                Some(tip) => store.writers_at(&tip)?,
                None => return deny("its namespace has no writers ref"),
            },
        };
        if !list.iter().any(|w| w.key == pusher.key) {
            return deny(&format!(
                "{} is not one of the namespace's writers",
                pusher.key
            ));
        }
        if let Some(scope) = &pusher.token {
            if !scope.covers(namespace) {
                return deny("this job's run token was not granted that namespace");
            }
        }
    }
    Ok(())
}

fn identify(
    commands: &[RefCommand],
    options: &[String],
    tokens: &dyn Tokens,
) -> Result<Pusher, String> {
    let mut auths = options.iter().filter_map(|o| Auth::parse_option(o));
    let auth = match (auths.next(), auths.next()) {
        (None, _) => {
            return Err(format!(
                "a governed ref needs a `{}` push option (a writer's signature or a run token)",
                writers::PUSH_OPTION
            ))
        }
        (Some(_), Some(_)) => {
            return Err(format!(
                "more than one `{}` push option",
                writers::PUSH_OPTION
            ))
        }
        (Some(auth), None) => auth?,
    };
    match auth {
        Auth::Signed {
            key,
            expiry,
            signature,
        } => {
            if expiry < writers::now() {
                return Err("the push's signature has expired".to_string());
            }
            writers::verify(&key, &update_message(expiry, commands), &signature)?;
            Ok(Pusher { key, token: None })
        }
        Auth::Run { token } => {
            let (key, scope) = tokens.lookup(&token)?;
            Ok(Pusher {
                key,
                token: Some(scope),
            })
        }
    }
}

struct Repo;

fn git(args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|_| "git printed non-UTF-8".to_string())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

impl Store for Repo {
    fn ref_value(&self, refname: &str) -> Option<String> {
        git(&["rev-parse", "--verify", "--quiet", refname])
            .ok()
            .map(|v| v.trim().to_string())
    }

    fn writers_at(&self, commit: &str) -> Result<Vec<Writer>, String> {
        let text = git(&["cat-file", "blob", &format!("{commit}:{WRITERS_PATH}")])
            .map_err(|_| format!("{commit} has no {WRITERS_PATH}"))?;
        parse_writers(&text)
    }

    fn parents(&self, commit: &str) -> Result<Vec<String>, String> {
        let text = git(&["cat-file", "commit", commit])?;
        Ok(text
            .lines()
            .take_while(|l| !l.is_empty())
            .filter_map(|l| l.strip_prefix("parent "))
            .map(str::to_string)
            .collect())
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        Command::new("git")
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .status()
            .is_ok_and(|s| s.success())
    }

    fn unguarded(&self) -> Vec<String> {
        git(&["config", "--get-all", UNGUARDED_CONFIG])
            .map(|v| v.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

struct TokenServer;

impl Tokens for TokenServer {
    fn lookup(&self, token: &str) -> Result<(String, Scope), String> {
        let server = std::env::var(SERVER_ENV)
            .map_err(|_| format!("{SERVER_ENV} is not set; cannot check a run token"))?;
        let response = minreq::post(format!("{server}{TOKEN_PATH}"))
            .with_body(token)
            .with_timeout(10)
            .send()
            .map_err(|e| format!("asking the server about a run token: {e}"))?;
        if response.status_code == 404 {
            return Err("unknown run token (its run has ended?)".to_string());
        }
        if response.status_code != 200 {
            return Err(format!(
                "the server answered {} about a run token",
                response.status_code
            ));
        }
        let body: serde_json::Value = serde_json::from_slice(response.as_bytes())
            .map_err(|e| format!("reading the run token's grant: {e}"))?;
        let key = body["key"].as_str().unwrap_or_default().to_string();
        if body["all"].as_bool() == Some(true) {
            return Ok((key, Scope::All));
        }
        let namespaces = body["namespaces"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok((key, Scope::Only(namespaces)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conversation_protocol::v3::writers::{genesis_id, WriterKey};

    #[derive(Default)]
    struct Fake {
        refs: HashMap<String, String>,
        writers: HashMap<String, Vec<Writer>>,
        parents: HashMap<String, Vec<String>>,
        ancestry: Vec<(String, String)>,
    }

    impl Store for Fake {
        fn ref_value(&self, refname: &str) -> Option<String> {
            self.refs.get(refname).cloned()
        }
        fn writers_at(&self, commit: &str) -> Result<Vec<Writer>, String> {
            self.writers
                .get(commit)
                .cloned()
                .ok_or_else(|| format!("no {commit}"))
        }
        fn parents(&self, commit: &str) -> Result<Vec<String>, String> {
            Ok(self.parents.get(commit).cloned().unwrap_or_default())
        }
        fn is_ancestor(&self, a: &str, d: &str) -> bool {
            self.ancestry.iter().any(|(x, y)| x == a && y == d)
        }
        fn unguarded(&self) -> Vec<String> {
            vec![DEFAULT_UNGUARDED.to_string()]
        }
    }

    struct Grants(Vec<(String, String, Option<Vec<String>>)>);

    impl Tokens for Grants {
        fn lookup(&self, token: &str) -> Result<(String, Scope), String> {
            self.0
                .iter()
                .find(|(t, _, _)| t == token)
                .map(|(_, k, n)| {
                    let scope = n.clone().map_or(Scope::All, Scope::Only);
                    (k.clone(), scope)
                })
                .ok_or_else(|| "unknown".to_string())
        }
    }

    fn writer(key: &WriterKey) -> Writer {
        Writer {
            key: key.public(),
            label: String::new(),
        }
    }

    fn oid(c: char) -> String {
        c.to_string().repeat(40)
    }

    /// A store holding namespace `ns` whose writers are `keys`.
    fn namespace(keys: &[&WriterKey]) -> (Fake, String) {
        let list: Vec<Writer> = keys.iter().map(|k| writer(k)).collect();
        let ns = genesis_id(&list, "test").to_string();
        let mut fake = Fake::default();
        fake.refs.insert(writers::writers_ref(&ns), ns.clone());
        fake.writers.insert(ns.clone(), list);
        (fake, ns)
    }

    fn signed(key: &WriterKey, commands: &[RefCommand]) -> Vec<String> {
        vec![key.sign_update(commands)]
    }

    #[test]
    fn content_named_and_unguarded_refs_need_no_proof() {
        let none = Grants(vec![]);
        let ok = RefCommand::new(
            &format!("refs/caos/req/{}", oid('a')),
            None,
            Some(&oid('a')),
        );
        assert!(check(&Fake::default(), &[ok], &[], &none).is_ok());
        let lie = RefCommand::new(
            &format!("refs/caos/req/{}", oid('a')),
            None,
            Some(&oid('b')),
        );
        assert!(check(&Fake::default(), &[lie], &[], &none).is_err());
        let dev = RefCommand::new("refs/caos/dev", None, Some(&oid('b')));
        assert!(check(&Fake::default(), &[dev], &[], &none).is_ok());
        let other = RefCommand::new("refs/heads/main", None, Some(&oid('b')));
        assert!(check(&Fake::default(), &[other], &[], &none).is_err());
    }

    #[test]
    fn a_writer_signature_writes_its_namespace_and_nobody_elses() {
        let alice = WriterKey::from_seed([1; 32]);
        let mallory = WriterKey::from_seed([2; 32]);
        let (store, ns) = namespace(&[&alice]);
        let head = [RefCommand::new(
            &format!("refs/caos/w/{ns}/head"),
            None,
            Some(&oid('c')),
        )];
        let none = Grants(vec![]);
        assert!(check(&store, &head, &signed(&alice, &head), &none).is_ok());
        assert!(check(&store, &head, &signed(&mallory, &head), &none).is_err());
        assert!(check(&store, &head, &[], &none).is_err());
        // A signature covers exactly the commands it was made for.
        let other = [RefCommand::new(
            &format!("refs/caos/w/{ns}/head"),
            None,
            Some(&oid('d')),
        )];
        assert!(check(&store, &other, &signed(&alice, &head), &none).is_err());
    }

    #[test]
    fn a_namespace_is_created_only_at_its_own_hash_by_a_listed_writer() {
        let alice = WriterKey::from_seed([1; 32]);
        let bob = WriterKey::from_seed([3; 32]);
        let list = vec![writer(&alice)];
        let ns = genesis_id(&list, "new").to_string();
        let mut store = Fake::default();
        store.writers.insert(ns.clone(), list);
        let create = [
            RefCommand::new(&writers::writers_ref(&ns), None, Some(&ns)),
            RefCommand::new(&format!("refs/caos/w/{ns}/head"), None, Some(&oid('c'))),
        ];
        let none = Grants(vec![]);
        assert!(check(&store, &create, &signed(&alice, &create), &none).is_ok());
        assert!(check(&store, &create, &signed(&bob, &create), &none).is_err());
        let squat = [RefCommand::new(
            &writers::writers_ref(&oid('e')),
            None,
            Some(&ns),
        )];
        assert!(check(&store, &squat, &signed(&alice, &squat), &none).is_err());
    }

    #[test]
    fn writers_change_forward_only_and_removal_takes_effect() {
        let alice = WriterKey::from_seed([1; 32]);
        let bob = WriterKey::from_seed([3; 32]);
        let (mut store, ns) = namespace(&[&alice, &bob]);
        let next = oid('f');
        store.writers.insert(next.clone(), vec![writer(&alice)]);
        store.ancestry.push((ns.clone(), next.clone()));
        let remove_bob = [RefCommand::new(
            &writers::writers_ref(&ns),
            Some(&ns),
            Some(&next),
        )];
        let none = Grants(vec![]);
        assert!(check(&store, &remove_bob, &signed(&bob, &remove_bob), &none).is_ok());
        store.refs.insert(writers::writers_ref(&ns), next.clone());
        let head = [RefCommand::new(
            &format!("refs/caos/w/{ns}/head"),
            None,
            Some(&oid('c')),
        )];
        assert!(check(&store, &head, &signed(&bob, &head), &none).is_err());
        assert!(check(&store, &head, &signed(&alice, &head), &none).is_ok());
        let rewind = [RefCommand::new(
            &writers::writers_ref(&ns),
            Some(&next),
            Some(&ns),
        )];
        assert!(check(&store, &rewind, &signed(&alice, &rewind), &none).is_err());
    }

    #[test]
    fn a_run_token_writes_only_granted_namespaces_and_never_the_list() {
        let alice = WriterKey::from_seed([1; 32]);
        let (store, ns) = namespace(&[&alice]);
        let grants = Grants(vec![
            ("aa".into(), alice.public(), Some(vec![ns.clone()])),
            ("bb".into(), alice.public(), Some(vec![oid('9')])),
            ("ff".into(), alice.public(), None),
        ]);
        let head = [RefCommand::new(
            &format!("refs/caos/w/{ns}/head"),
            None,
            Some(&oid('c')),
        )];
        let run = |t: &str| vec![Auth::Run { token: t.into() }.option()];
        assert!(check(&store, &head, &run("aa"), &grants).is_ok());
        assert!(check(&store, &head, &run("bb"), &grants).is_err());
        assert!(check(&store, &head, &run("cc"), &grants).is_err());
        assert!(check(&store, &head, &run("ff"), &grants).is_ok());
        let list = [RefCommand::new(
            &writers::writers_ref(&ns),
            Some(&ns),
            Some(&oid('f')),
        )];
        assert!(check(&store, &list, &run("aa"), &grants).is_err());
        assert!(check(&store, &list, &run("ff"), &grants).is_err());
    }

    #[test]
    fn a_run_token_may_found_a_namespace_for_its_writer() {
        let alice = WriterKey::from_seed([1; 32]);
        let bob = WriterKey::from_seed([3; 32]);
        let list = vec![writer(&alice)];
        let ns = genesis_id(&list, "job").to_string();
        let mut store = Fake::default();
        store.writers.insert(ns.clone(), list);
        let create = [
            RefCommand::new(&writers::writers_ref(&ns), None, Some(&ns)),
            RefCommand::new(&format!("refs/caos/w/{ns}/head"), None, Some(&oid('c'))),
        ];
        let grants = Grants(vec![
            ("ff".into(), alice.public(), None),
            ("b0b".into(), bob.public(), None),
            ("eee".into(), alice.public(), Some(vec![oid('9')])),
        ]);
        let run = |t: &str| vec![Auth::Run { token: t.into() }.option()];
        assert!(check(&store, &create, &run("ff"), &grants).is_ok());
        assert!(check(&store, &create, &run("b0b"), &grants).is_err());
        assert!(check(&store, &create, &run("eee"), &grants).is_err());
    }

    #[test]
    fn admission_takes_only_a_live_signature_over_this_request() {
        let alice = WriterKey::from_seed([1; 32]);
        let header = alice.sign_admission("GET", "/run?x");
        assert!(matches!(
            admit(Some(&header), "GET", "/run?x"),
            Ok(Writes::As { scope: None, .. })
        ));
        assert!(admit(Some(&header), "GET", "/run?y").is_err());
        assert!(admit(Some(&header), "POST", "/run?x").is_err());
        assert!(matches!(admit(None, "GET", "/run?x"), Ok(Writes::None)));
    }

    #[test]
    fn hook_reaches_the_server_on_loopback() {
        assert_eq!(hook_server_url("[::]:80"), "http://127.0.0.1:80");
        assert_eq!(hook_server_url("127.0.0.1:4567"), "http://127.0.0.1:4567");
    }
}
