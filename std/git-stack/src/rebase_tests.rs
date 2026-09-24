use super::*;
use conversation_protocol::v3::{Kind, MemoryStore, Signature};

fn signature(time: i64) -> Signature {
    Signature {
        name: "Replay".into(),
        email: "replay@example.com".into(),
        time,
        offset: "+0000".into(),
    }
}

fn source(store: &mut MemoryStore, value: &str, parents: Vec<Oid>, time: i64) -> Oid {
    let mut builder = TreeBuilder::from(None);
    builder.put("code", Mode::Blob, value.as_bytes().to_vec());
    let tree = builder.build(store).unwrap();
    store
        .write_commit(&CommitInfo {
            tree,
            parents,
            author: signature(time),
            committer: signature(time),
            extra_headers: Vec::new(),
            message: format!("source {time}\n").into_bytes(),
        })
        .unwrap()
}

fn history(store: &mut MemoryStore, tree: &Oid, parent: Option<&Oid>, records: Vec<Event>) -> Oid {
    store
        .write_commit(&CommitInfo {
            tree: tree.clone(),
            parents: parent.into_iter().cloned().collect(),
            author: signature(1),
            committer: signature(1),
            extra_headers: Vec::new(),
            message: events::encode(
                if records.is_empty() {
                    Kind::FilesApply
                } else {
                    Kind::ToolComplete
                },
                &records,
            ),
        })
        .unwrap()
}

fn accepted_event(scope: &str, applied: bool, report: serde_json::Value) -> Event {
    Event::Payload {
        path: ".caos/tools/test/0/call/writer-result.json".into(),
        bytes: serde_json::to_vec(
            &json!({"scope":scope, "applied":applied, "report":report.to_string()}),
        )
        .unwrap(),
    }
}

fn starting() -> Parameters {
    Parameters {
        stack: "feature".into(),
        plan: Some("feature/rebase/plan".into()),
        action: None,
    }
}

fn continuing() -> Parameters {
    Parameters {
        stack: "feature".into(),
        plan: None,
        action: Some("continue".into()),
    }
}

struct Fixture {
    store: MemoryStore,
    root: Oid,
    head: Oid,
    base: Oid,
    first: Oid,
    tip: Oid,
}

impl Fixture {
    fn new() -> Self {
        let mut store = MemoryStore::new();
        let base = source(&mut store, "base", vec![], 1);
        let first = source(&mut store, "first change", vec![base.clone()], 2);
        let tip = source(&mut store, "second change", vec![first.clone()], 3);
        let mut tree = TreeBuilder::from(None);
        tree.put_oid("feature/00-work", Mode::Commit, tip.clone());
        tree.put("feature/00.base", Mode::Blob, base.encode_line());
        tree.put("feature/notes", Mode::Blob, b"original notes".to_vec());
        tree.put("elsewhere", Mode::Blob, b"unrelated".to_vec());
        tree.put(
            "feature/rebase/plan",
            Mode::Blob,
            format!("onto={base}\nbranch=00-empty\n").into_bytes(),
        );
        let root = tree.build(&mut store).unwrap();
        let head = history(&mut store, &root, None, vec![]);
        Self {
            store,
            root,
            head,
            base,
            first,
            tip,
        }
    }

    fn context(&self) -> Context {
        Context {
            head: self.head.clone(),
            scope: "feature".into(),
            committer: "Replay <replay@example.com> 99 +0000".into(),
        }
    }

    fn edit(&mut self, change: impl FnOnce(&mut TreeBuilder)) {
        let mut tree = TreeBuilder::from(Some(self.root.clone()));
        change(&mut tree);
        self.root = tree.build(&mut self.store).unwrap();
        self.head = history(&mut self.store, &self.root, Some(&self.head), vec![]);
    }

    fn report_commit(&mut self, event: Event) {
        self.head = history(&mut self.store, &self.root, Some(&self.head), vec![event]);
    }

    /// Install an accepted conflict result without invoking an external merger.
    fn pause(&mut self, branch_first: bool) -> (String, Oid) {
        let original = stack::read_stack(&self.store, &self.root, "feature")
            .unwrap()
            .numbered_tree(&mut self.store)
            .unwrap();
        let mut layers = TreeBuilder::from(None);
        let prefix = if branch_first {
            layers.put_oid("00-first", Mode::Commit, self.first.clone());
            layers.put("00.base", Mode::Blob, self.base.encode_line());
            format!(
                "done {} pick={}\ndone {} branch=00-first\n",
                self.first, self.first, self.first
            )
        } else {
            String::new()
        };
        let layers = layers.build(&mut self.store).unwrap();
        let selected = if branch_first {
            self.tip.to_string()
        } else {
            format!("{}..{}", self.base, self.tip)
        };
        let branch = if branch_first { "01-second" } else { "00-all" };
        let plan = format!(
            "onto={}\noriginal {original}\ncommitter {{\"name\":\"Replay\",\"email\":\"replay@example.com\",\"time\":99,\"offset\":\"+0000\"}}\n{prefix}here conflict pick={selected}\npick={selected}\nbranch={branch}\n", self.base,
        );
        let parent = if branch_first {
            self.first.clone()
        } else {
            self.base.clone()
        };
        let work = source(&mut self.store, "resolved draft", vec![parent], 100);
        let draft_tree = self.store.read_commit(&work).unwrap().tree;
        let mut tree = TreeBuilder::from(Some(self.root.clone()));
        tree.put("feature/rebase/plan", Mode::Blob, plan.as_bytes().to_vec());
        if branch_first {
            tree.put_oid("feature/rebase/stack", Mode::Tree, layers.clone());
        }
        tree.put_oid("feature/rebase/work", Mode::Commit, work);
        tree.put(
            "feature/rebase/conflicts",
            Mode::Blob,
            b"native conflict report\0".to_vec(),
        );
        self.root = tree.build(&mut self.store).unwrap();
        let report =
            json!({"kind":"git-rebase-i", "stack":"feature", "plan":plan, "output_tree":layers});
        self.report_commit(accepted_event("feature", true, report));
        (plan, draft_tree)
    }

    fn propose(&mut self, parameters: &Parameters) -> Result<Proposal, String> {
        let context = self.context();
        propose(&mut self.store, &self.root, &context, parameters)
    }
}

#[test]
fn start_proposes_empty_layer_and_preserves_input_and_ordinary_files() {
    let mut f = Fixture::new();
    let original_root = f.root.clone();
    let proposal = f.propose(&starting()).unwrap();
    let output = stack::read_stack(&f.store, &proposal.tree, "feature").unwrap();
    assert_eq!(output.layers.len(), 1);
    assert_eq!(output.layers[0].name, "00-empty");
    assert_eq!(output.layers[0].commit, f.base);
    assert_eq!(output.layers[0].base, f.base);
    let after = Snapshot::new(&f.store, proposal.tree);
    assert_eq!(
        after.read("feature/notes").unwrap().unwrap(),
        b"original notes"
    );
    assert_eq!(after.read("elsewhere").unwrap().unwrap(), b"unrelated");
    assert!(!after.exists("feature/rebase").unwrap());
    assert!(!after.exists("feature/00-work").unwrap());
    assert!(Snapshot::new(&f.store, original_root)
        .exists("feature/rebase/plan")
        .unwrap());
    assert_eq!(proposal.report["kind"], "git-rebase-i");
    assert_eq!(proposal.report["status"], "complete");
    let executed = rebase_plan::Plan::parse(proposal.report["plan"].as_str().unwrap()).unwrap();
    assert_eq!(executed.committer, Some(signature(99)));
}

#[test]
fn continuation_uses_draft_tree_and_trusted_saved_layers_without_remerging() {
    let mut f = Fixture::new();
    let (_, draft_tree) = f.pause(true);
    let mut context = f.context();
    context.committer = "Different <different@example.com> 999 +0100".into();
    let proposal = propose(&mut f.store, &f.root, &context, &continuing()).unwrap();
    let output = stack::read_stack(&f.store, &proposal.tree, "feature").unwrap();
    assert_eq!(output.layers[0].name, "00-first");
    assert_eq!(output.layers[0].commit, f.first);
    assert_eq!(output.layers[1].name, "01-second");
    assert_eq!(output.layers[1].base, f.first);
    let commit = f.store.read_commit(&output.layers[1].commit).unwrap();
    assert_eq!(commit.tree, draft_tree);
    assert_eq!(commit.parents, vec![f.first]);
    assert_eq!(commit.author, signature(3));
    assert_eq!(
        commit.committer,
        signature(99),
        "replay committer remains fixed across calls"
    );
    assert_eq!(commit.message, b"source 3\n");
    assert!(!Snapshot::new(&f.store, proposal.tree)
        .exists("feature/rebase")
        .unwrap());
    assert_eq!(proposal.report["status"], "complete");
}

#[test]
fn no_finished_layers_yet_can_continue_with_absent_output_directory() {
    let mut f = Fixture::new();
    let (_, draft_tree) = f.pause(false);
    assert!(!Snapshot::new(&f.store, f.root.clone())
        .exists("feature/rebase/stack")
        .unwrap());
    let proposal = f.propose(&continuing()).unwrap();
    let output = stack::read_stack(&f.store, &proposal.tree, "feature").unwrap();
    assert_eq!(output.layers[0].name, "00-all");
    let commit = f.store.read_commit(&output.layers[0].commit).unwrap();
    assert_eq!(commit.parents, vec![f.base]);
    assert_eq!(commit.tree, draft_tree);
}

#[test]
fn replaced_paused_instruction_does_not_require_its_old_draft() {
    let mut f = Fixture::new();
    let (plan, _) = f.pause(false);
    let changed = plan.replace(&format!("\npick={}..{}\n", f.base, f.tip), "\n");
    f.edit(|tree| {
        tree.put("feature/rebase/plan", Mode::Blob, changed.into_bytes());
        tree.delete("feature/rebase/work");
    });
    let proposal = f.propose(&continuing()).unwrap();
    let output = stack::read_stack(&f.store, &proposal.tree, "feature").unwrap();
    assert_eq!(output.layers[0].commit, f.base);
    assert_eq!(output.layers[0].base, f.base);
}

#[test]
fn completed_instruction_and_recorded_layer_tampering_are_rejected() {
    let mut f = Fixture::new();
    let (plan, _) = f.pause(true);
    let changed = plan.replace(
        &format!("done {} pick={}", f.first, f.first),
        &format!("done {} pick={}..{}", f.first, f.base, f.first),
    );
    f.edit(|tree| tree.put("feature/rebase/plan", Mode::Blob, changed.into_bytes()));
    assert!(f.propose(&continuing()).unwrap_err().contains("fixed"));

    for wrong_mode in [false, true] {
        let mut f = Fixture::new();
        f.pause(true);
        let changed = f.tip.clone();
        f.edit(|tree| {
            if wrong_mode {
                tree.delete("feature/rebase/stack");
                tree.put("feature/rebase/stack", Mode::Blob, b"wrong mode".to_vec());
            } else {
                tree.put_oid("feature/rebase/stack/00-first", Mode::Commit, changed);
            }
        });
        let error = f.propose(&continuing()).unwrap_err();
        assert!(error.contains("output layers"), "{error}");
    }
}

#[test]
fn changed_original_numbered_entries_fail_without_changing_the_input() {
    for change_base in [false, true] {
        let mut f = Fixture::new();
        f.pause(true);
        let changed = f.first.clone();
        f.edit(|tree| {
            if change_base {
                tree.put("feature/00.base", Mode::Blob, changed.encode_line());
            } else {
                tree.put_oid("feature/00-work", Mode::Commit, changed);
            }
        });
        let before = f.root.clone();
        assert!(f
            .propose(&continuing())
            .unwrap_err()
            .contains("original numbered stack entries changed"));
        assert_eq!(f.root, before);
        let input = Snapshot::new(&f.store, before);
        assert!(input.exists("feature/rebase/work").unwrap());
        assert!(input.exists("feature/rebase/plan").unwrap());
        assert!(!input.exists("feature/00-first").unwrap());
    }
}

#[test]
fn ordinary_files_changed_between_calls_survive_completion() {
    let mut f = Fixture::new();
    f.pause(true);
    f.edit(|tree| {
        tree.put(
            "feature/notes",
            Mode::Blob,
            b"notes written while resolving".to_vec(),
        );
        tree.put("elsewhere", Mode::Blob, b"other work".to_vec());
    });
    let proposal = f.propose(&continuing()).unwrap();
    let output = Snapshot::new(&f.store, proposal.tree);
    assert_eq!(
        output.read("feature/notes").unwrap().unwrap(),
        b"notes written while resolving"
    );
    assert_eq!(output.read("elsewhere").unwrap().unwrap(), b"other work");
}

#[test]
fn checkpoint_ignores_unapplied_and_other_stack_reports() {
    let mut f = Fixture::new();
    let (plan, _) = f.pause(true);
    f.report_commit(accepted_event(
        "feature",
        false,
        json!({"kind":"git-rebase-i", "stack":"feature", "plan":"not accepted"}),
    ));
    f.report_commit(accepted_event(
        "other",
        true,
        json!({"kind":"git-rebase-i", "stack":"other", "plan":"other stack"}),
    ));
    f.report_commit(accepted_event(
        "feature",
        true,
        json!({"kind":"different-writer", "stack":"feature", "plan":"different tool"}),
    ));
    let prior = checkpoint(&f.store, &f.head, "feature").unwrap().unwrap();
    assert_eq!(prior.plan, plan);
    assert!(f.propose(&continuing()).is_ok());
}

#[test]
fn abort_and_recreation_clear_old_checkpoint_and_allow_a_new_start() {
    let mut f = Fixture::new();
    f.pause(true);
    f.edit(|tree| tree.delete("feature/rebase"));
    let plan = format!("onto={}\nbranch=00-restarted\n", f.base);
    f.edit(|tree| tree.put("feature/rebase/plan", Mode::Blob, plan.into_bytes()));
    assert!(checkpoint(&f.store, &f.head, "feature").unwrap().is_none());
    assert!(f
        .propose(&continuing())
        .unwrap_err()
        .contains("no started replay"));
    let proposal = f.propose(&starting()).unwrap();
    assert_eq!(proposal.report["layers"][0]["name"], "00-restarted");
}

#[test]
fn external_plan_requires_copying_to_the_replay_directory() {
    let mut f = Fixture::new();
    let plan = format!("onto={}\nbranch=00-copied\n", f.base);
    f.edit(|tree| {
        tree.delete("feature/rebase");
        tree.put("plans/feature.todo", Mode::Blob, plan.as_bytes().to_vec());
    });
    assert!(!Snapshot::new(&f.store, f.root.clone())
        .exists("feature/rebase")
        .unwrap());
    for path in [
        "plans/feature.todo",
        "./feature/rebase/plan",
        "feature/rebase/../rebase/plan",
    ] {
        let parameters = Parameters {
            plan: Some(path.into()),
            ..starting()
        };
        assert_eq!(
            f.propose(&parameters).unwrap_err(),
            "copy the plan to feature/rebase/plan first, then use plan=feature/rebase/plan"
        );
    }
    f.edit(|tree| tree.put("feature/rebase/plan", Mode::Blob, plan.into_bytes()));
    let proposal = f.propose(&starting()).unwrap();
    assert_eq!(proposal.report["layers"][0]["name"], "00-copied");
    let output = Snapshot::new(&f.store, proposal.tree);
    assert!(output.exists("plans/feature.todo").unwrap());
    assert!(!output.exists("feature/rebase").unwrap());
}

#[test]
fn start_and_continue_reject_invalid_arguments_or_operation_state() {
    let mut f = Fixture::new();
    for parameters in [
        Parameters {
            stack: "feature".into(),
            plan: None,
            action: None,
        },
        Parameters {
            stack: "feature".into(),
            plan: starting().plan,
            action: Some("continue".into()),
        },
        Parameters {
            stack: "feature".into(),
            plan: None,
            action: Some("skip".into()),
        },
    ] {
        assert!(f.propose(&parameters).unwrap_err().contains("provide plan"));
    }
    assert!(f
        .propose(&continuing())
        .unwrap_err()
        .contains("no started replay"));
    f.edit(|tree| {
        tree.put(
            "feature/rebase/stray",
            Mode::Blob,
            b"leftover state".to_vec(),
        )
    });
    assert!(f
        .propose(&starting())
        .unwrap_err()
        .contains("only its plan"));
    let mut f = Fixture::new();
    f.pause(true);
    assert!(f
        .propose(&starting())
        .unwrap_err()
        .contains("already has an active replay"));
}

#[test]
fn writer_context_must_match_scope_snapshot_and_explicit_signature() {
    let mut f = Fixture::new();
    let mut context = f.context();
    context.scope = "other".into();
    assert!(propose(&mut f.store, &f.root, &context, &starting())
        .unwrap_err()
        .contains("different stack"));
    let context = f.context();
    f.edit(|tree| tree.put("elsewhere", Mode::Blob, b"new tree".to_vec()));
    assert!(propose(&mut f.store, &f.root, &context, &starting())
        .unwrap_err()
        .contains("recorded conversation snapshot"));
    let mut context = f.context();
    context.committer = "implicit default author".into();
    assert!(propose(&mut f.store, &f.root, &context, &starting())
        .unwrap_err()
        .contains("signature"));
}
