//! One external CLI execution per invocation; unfinished claims are uncertain.
use conversation_protocol::v3::{
    CodeOps, GitStore, Mode, ObjectStore, Oid, RefUpdate, Signature, TreeEntry,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    io::Read,
    process::{Command, Stdio},
};
use worker_common::{caos, cas_hash, own_args_tree, path, read_arg, run_worker, scratch, secret};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    request: String,
    attempt: String,
    result: Option<Oid>,
}

fn main() -> std::process::ExitCode {
    run_worker("github", run)
}

fn run() -> Result<(), String> {
    let repository = read_arg("repository")?;
    git_locator::github_repository(&repository)?;
    let args: Vec<String> =
        serde_json::from_str(&read_arg("args")?).map_err(|_| "args must be a JSON string array")?;
    if args.is_empty() || args.iter().any(|a| a.contains('\0')) {
        return Err("invalid gh arguments".into());
    }
    let input_arg = worker_common::arg("stdin");
    let stdin = if std::path::Path::new(&input_arg).exists() {
        caos(["get", &input_arg])?;
        fs::read(&input_arg).map_err(|e| format!("reading stdin: {e}"))?
    } else {
        Vec::new()
    };
    let invocation = read_arg("invocation")?;
    if invocation.len() != 64
        || !invocation
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invocation must be 64 lowercase hexadecimal characters".into());
    }
    let request = own_args_tree()?;
    let token = secret("github-token")?;
    let server = std::env::var("CAOS_SERVER_URL").map_err(|_| "CAOS_SERVER_URL not set")?;
    let mut store = GitStore::scratch("github-invocation", &server)?;
    let refname = format!("refs/caos/github/{invocation}");
    let mut random = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut random))
        .map_err(|e| e.to_string())?;
    let attempt = random.iter().map(|b| format!("{b:02x}")).collect();
    let (claim, mut record) = match acquire(&mut store, &refname, request, attempt)? {
        Acquisition::Owned(head, record) => (head, record),
        Acquisition::Complete(result) => return return_result(&result),
        Acquisition::Uncertain => return uncertain(),
    };

    let dir = scratch("github-command")?;
    let config = dir.join("config");
    let data = dir.join("data");
    fs::create_dir_all(data.join("gh/extensions/gh-stack")).map_err(|e| e.to_string())?;
    std::os::unix::fs::symlink(
        "/opt/gh-stack",
        data.join("gh/extensions/gh-stack/gh-stack"),
    )
    .map_err(|e| e.to_string())?;
    // A file avoids pipe deadlocks for large stdin while gh writes output.
    let input = dir.join("stdin");
    fs::write(&input, stdin).map_err(|e| e.to_string())?;
    let mut cmd = Command::new("timeout");
    cmd.args(["--kill-after=5", "300", "gh"])
        .args(&args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &dir)
        .env("GH_CONFIG_DIR", &config)
        .env("XDG_DATA_HOME", &data)
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
        .env("GH_REPO", &repository)
        .env("GH_HOST", "github.com")
        .env("GH_TOKEN", &token)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(&dir)
        .stdin(Stdio::from(
            fs::File::open(&input).map_err(|e| e.to_string())?,
        ));
    if let Some(cert) = std::env::var_os("SSL_CERT_FILE") {
        cmd.env("SSL_CERT_FILE", cert);
    }
    let result = match cmd.output() {
        Ok(output) => {
            json!({"status":if matches!(output.status.code(), None | Some(124 | 137)) { "uncertain" } else { "complete" },"exit":output.status.code(),
            "stdout":String::from_utf8_lossy(&output.stdout), "stderr":String::from_utf8_lossy(&output.stderr)})
        }
        Err(_) => {
            json!({"status":"uncertain","exit":null,"stdout":"","stderr":"Could not obtain a GitHub command result; inspect before taking further action."})
        }
    };
    let result_file = dir.join("result.json");
    fs::write(
        &result_file,
        serde_json::to_vec(&result).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    // Retry only durable bookkeeping; the gh command above is never repeated.
    let saved = retry_bookkeeping(|| {
        // Normal object upload scrubs secrets before storing command output.
        caos(["put", path(&result_file), "/cas/github-result"])?;
        let oid = Oid::parse(&cas_hash("/cas/github-result")?, "GitHub result")?;
        store.ensure_local(&oid)?;
        record.result = Some(oid);
        let done = commit(&mut store, &record, Some(&claim))?;
        if store
            .push(&[RefUpdate {
                refname: refname.clone(),
                expected: Some(claim.clone()),
                new: Some(done.clone()),
            }])
            .is_err()
            && store.fetch_ref(&refname)? != Some(done)
        {
            return Err("could not retain GitHub result".into());
        }
        Ok(())
    });
    if saved.is_err() {
        return uncertain();
    }
    worker_common::forward("/cas/github-result", "/cas/out")
}

fn commit(store: &mut GitStore, record: &Record, parent: Option<&Oid>) -> Result<Oid, String> {
    let entries = record
        .result
        .iter()
        .map(|oid| TreeEntry {
            name: "result.json".into(),
            mode: Mode::Blob,
            oid: oid.clone(),
        })
        .collect::<Vec<_>>();
    let tree = store.write_tree(&entries).map_err(String::from)?;
    let signature = Signature {
        name: "caos".into(),
        email: "caos@caos".into(),
        time: 0,
        offset: "+0000".into(),
    };
    store.commit(
        &tree,
        &parent.cloned().into_iter().collect::<Vec<_>>(),
        &serde_json::to_string(record).map_err(|e| e.to_string())?,
        &signature,
    )
}

enum Acquisition {
    Owned(Oid, Record),
    Complete(Oid),
    Uncertain,
}

fn acquire(
    store: &mut GitStore,
    refname: &str,
    request: String,
    attempt: String,
) -> Result<Acquisition, String> {
    if let Some(head) = store.fetch_ref(refname)? {
        return observed(store, &head, &request);
    }
    let record = Record {
        request,
        attempt,
        result: None,
    };
    let claim = commit(store, &record, None)?;
    if store
        .push(&[RefUpdate {
            refname: refname.into(),
            expected: None,
            new: Some(claim.clone()),
        }])
        .is_err()
    {
        // Only this attempt can recover ownership after a lost push response.
        match store.fetch_ref(refname)? {
            Some(head) if head == claim => {}
            Some(head) => return observed(store, &head, &record.request),
            None => {
                return Err("GitHub invocation claim was not retained; retry the request".into())
            }
        }
    }
    Ok(Acquisition::Owned(claim, record))
}

fn retry_bookkeeping(mut save: impl FnMut() -> Result<(), String>) -> Result<(), String> {
    for attempt in 0..3 {
        match save() {
            Ok(()) => return Ok(()),
            Err(error) if attempt == 2 => return Err(error),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(100 << attempt)),
        }
    }
    unreachable!()
}

fn observed(store: &GitStore, head: &Oid, request: &str) -> Result<Acquisition, String> {
    let info = store.read_commit(head).map_err(String::from)?;
    let record: Record =
        serde_json::from_slice(&info.message).map_err(|_| "invalid GitHub invocation record")?;
    if record.request != request {
        return Err("GitHub invocation already belongs to a different request".into());
    }
    Ok(match record.result {
        Some(result) => Acquisition::Complete(result),
        None => Acquisition::Uncertain,
    })
}

fn return_result(result: &Oid) -> Result<(), String> {
    caos(["get-hash", result.as_str(), "/cas/out"]).map(|_| ())
}

fn uncertain() -> Result<(), String> {
    let dir = scratch("github-uncertain")?;
    let file = dir.join("result.json");
    fs::write(&file, json!({"status":"uncertain","exit":null,"stdout":"",
        "stderr":"This invocation was claimed without a confirmed result. It may still be running. It was not repeated. Use a new read invocation to inspect GitHub; absence alone does not prove a write failed."}).to_string())
        .map_err(|e| e.to_string())?;
    caos(["put", path(&file), "/cas/out"]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn one_execution_survives_competing_attempts_restart_and_command_failure() {
        let directory = std::env::temp_dir().join(format!(
            "github-cas-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        assert!(Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&directory)
            .status()
            .unwrap()
            .success());
        let hook = directory.join("hooks/pre-receive");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let mut blocked =
            GitStore::scratch("github-test-blocked", directory.to_str().unwrap()).unwrap();
        assert!(acquire(
            &mut blocked,
            "refs/caos/github/call",
            "request".into(),
            "blocked".into()
        )
        .is_err());
        assert_eq!(blocked.fetch_ref("refs/caos/github/call").unwrap(), None);
        fs::remove_file(&hook).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let mut threads = Vec::new();
        for i in 0..8 {
            let remote = directory.to_string_lossy().to_string();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                let mut store = GitStore::scratch(&format!("github-test-{i}"), &remote).unwrap();
                barrier.wait();
                match acquire(
                    &mut store,
                    "refs/caos/github/call",
                    "request".into(),
                    i.to_string(),
                )
                .unwrap()
                {
                    Acquisition::Owned(head, record) => Some((store, head, record)),
                    Acquisition::Uncertain => None,
                    Acquisition::Complete(_) => panic!("no command has finished"),
                }
            }));
        }
        let mut owners = threads
            .into_iter()
            .filter_map(|t| t.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            owners.len(),
            1,
            "a retried worker would execute the command twice"
        );
        let (mut owner, claim, mut record) = owners.pop().unwrap();
        let mut resumed =
            GitStore::scratch("github-test-resumed", directory.to_str().unwrap()).unwrap();
        assert!(matches!(
            acquire(
                &mut resumed,
                "refs/caos/github/call",
                "request".into(),
                "restart".into()
            )
            .unwrap(),
            Acquisition::Uncertain
        ));
        // A failed multi-step command still has a durable result; it is not rerun.
        let result = owner
            .write_blob(
                br#"{"status":"complete","exit":1,"stdout":"created PR","stderr":"link failed"}"#,
            )
            .unwrap();
        record.result = Some(result.clone());
        let done = commit(&mut owner, &record, Some(&claim)).unwrap();
        fs::write(directory.join("bookkeeping-attempts"), "0\n").unwrap();
        fs::write(&hook, "#!/bin/sh\nn=$(cat bookkeeping-attempts)\nn=$((n + 1))\nprintf '%s\\n' \"$n\" > bookkeeping-attempts\nif [ \"$n\" -lt 3 ]; then exit 1; fi\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        retry_bookkeeping(|| {
            owner.push(&[RefUpdate {
                refname: "refs/caos/github/call".into(),
                expected: Some(claim.clone()),
                new: Some(done.clone()),
            }])
        })
        .unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("bookkeeping-attempts")).unwrap(),
            "3\n"
        );
        fs::remove_file(&hook).unwrap();
        assert!(
            matches!(acquire(&mut resumed, "refs/caos/github/call", "request".into(), "retry".into()).unwrap(),
            Acquisition::Complete(hash) if hash == result)
        );
        assert!(acquire(
            &mut resumed,
            "refs/caos/github/call",
            "different request or secret identity".into(),
            "other".into()
        )
        .is_err());
        // Same command in a new invocation gets a new observation.
        assert!(matches!(
            acquire(
                &mut resumed,
                "refs/caos/github/next",
                "request-2".into(),
                "next".into()
            )
            .unwrap(),
            Acquisition::Owned(..)
        ));
        fs::remove_dir_all(directory).unwrap();
    }
}
