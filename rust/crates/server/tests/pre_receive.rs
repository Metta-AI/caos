//! The ref-writers hook end to end: real `git push`es, with push options, into a
//! bare repository whose pre-receive hook is this crate's binary
//! (design/ref-writers.md). The unit tests in `ref_writers` cover the rules;
//! this covers reading a pushed `writers` list out of git's quarantine.

use std::path::{Path, PathBuf};
use std::process::Command;

use conversation_protocol::v3::writers::{self, genesis_commit, Writer, WriterKey};
use conversation_protocol::v3::{GitStore, ObjectStore, Oid, RefUpdate};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn server_repo(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("caos-pre-receive-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    git(
        Path::new("/"),
        &["init", "-q", "--bare", dir.to_str().unwrap()],
    );
    git(&dir, &["config", "receive.advertisePushOptions", "true"]);
    let hook = dir.join("hooks").join("pre-receive");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nexec '{}' --pre-receive\n",
            env!("CARGO_BIN_EXE_server")
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn client(name: &str, server: &Path, key: &'static WriterKey) -> (PathBuf, GitStore) {
    let dir = std::env::temp_dir().join(format!(
        "caos-pre-receive-client-{name}-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    git(
        Path::new("/"),
        &["init", "-q", "--bare", dir.to_str().unwrap()],
    );
    git(&dir, &["remote", "add", "caos", server.to_str().unwrap()]);
    let mut store = GitStore::open(&dir, Some("caos")).unwrap();
    store.set_push_auth(Box::new(move |commands: &[writers::Command]| {
        key.sign_update(commands)
    }));
    (dir, store)
}

fn update(refname: &str, expected: Option<&Oid>, new: &Oid) -> RefUpdate {
    RefUpdate {
        refname: refname.to_string(),
        expected: expected.cloned(),
        new: Some(new.clone()),
    }
}

#[test]
fn only_a_namespaces_writers_can_push_into_it() {
    static ALICE: std::sync::LazyLock<WriterKey> =
        std::sync::LazyLock::new(|| WriterKey::from_seed([1; 32]));
    static BOB: std::sync::LazyLock<WriterKey> =
        std::sync::LazyLock::new(|| WriterKey::from_seed([2; 32]));
    let server = server_repo("writers");
    let (alice_dir, mut alice) = client("alice", &server, &ALICE);
    let (bob_dir, bob) = client("bob", &server, &BOB);

    let list = vec![Writer {
        key: ALICE.public(),
        label: "alice".into(),
    }];
    let ns = genesis_commit(&mut alice, &list, "test").unwrap();
    let tree = alice.write_tree(&[]).unwrap();
    let content = alice
        .write_commit(&conversation_protocol::v3::CommitInfo {
            tree,
            parents: vec![],
            author: sig(),
            committer: sig(),
            extra_headers: vec![],
            message: b"content\n".to_vec(),
        })
        .unwrap();
    let head = format!("refs/caos/w/{ns}/head");

    // Creating the namespace and a ref in it, in one atomic push.
    alice
        .push(&[
            update(&writers::writers_ref(ns.as_str()), None, &ns),
            update(&head, None, &ns),
        ])
        .expect("alice creates her namespace");

    // Bob is not a writer.
    bob.fetch_object(&ns).unwrap();
    let refused = bob.push(&[update(&format!("refs/caos/w/{ns}/other"), None, &ns)]);
    let refused = refused.expect_err("bob wrote alice's namespace");
    assert!(
        refused.contains("not one of the namespace's writers"),
        "{refused}"
    );

    // Alice adds Bob; now he can write.
    let with_bob = writers::writers_commit(
        &mut alice,
        &ns,
        &[
            list[0].clone(),
            Writer {
                key: BOB.public(),
                label: "bob".into(),
            },
        ],
        &sig(),
        "add bob",
    )
    .unwrap();
    alice
        .push(&[update(
            &writers::writers_ref(ns.as_str()),
            Some(&ns),
            &with_bob,
        )])
        .expect("alice adds bob");
    alice
        .push(&[update(&head, Some(&ns), &content)])
        .expect("alice advances head");
    bob.fetch_object(&content).unwrap();
    bob.push(&[update(&format!("refs/caos/w/{ns}/other"), None, &content)])
        .expect("bob writes once added");

    for dir in [server, alice_dir, bob_dir] {
        std::fs::remove_dir_all(dir).ok();
    }
}

fn sig() -> conversation_protocol::v3::Signature {
    conversation_protocol::v3::Signature {
        name: "t".into(),
        email: "t@t".into(),
        time: 1,
        offset: "+0000".into(),
    }
}
