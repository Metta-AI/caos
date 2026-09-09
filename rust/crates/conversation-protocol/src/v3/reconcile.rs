use super::oid::Oid;
use super::records::{MergeInfo, WorkspaceResolution};
use super::tree::Signature;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged { tree: Oid },
    Conflict { paths: Vec<String> },
}

pub trait CodeOps {
    fn is_ancestor(&self, ancestor: &Oid, descendant: &Oid) -> Result<bool, String>;
    fn tree_of(&self, commit: &Oid) -> Result<Oid, String>;
    fn merge(&mut self, base: &Oid, ours: &Oid, theirs: &Oid) -> Result<MergeOutcome, String>;
    fn commit(
        &mut self,
        tree: &Oid,
        parents: &[Oid],
        message: &str,
        signature: &Signature,
    ) -> Result<Oid, String>;
    fn implementation(&self) -> String;
}

pub const RECONCILE_MESSAGE: &str = "reconcile\n";

pub fn reconcile(
    ops: &mut dyn CodeOps,
    base: &Oid,
    proposal: &Oid,
    current: Option<&Oid>,
    signature: &Signature,
) -> Result<WorkspaceResolution, String> {
    let Some(current) = current else {
        return Ok(WorkspaceResolution::Conflict {
            current: None,
            candidate: proposal.clone(),
            merge: None,
        });
    };
    if !ops.is_ancestor(base, proposal)? {
        return Err(format!(
            "proposal {proposal} does not descend from its base {base}"
        ));
    }
    if proposal == base || proposal == current || ops.is_ancestor(proposal, current)? {
        return Ok(WorkspaceResolution::AlreadyApplied {
            current: current.clone(),
            candidate: None,
        });
    }
    if ops.tree_of(proposal)? == ops.tree_of(current)? {
        return Ok(WorkspaceResolution::AlreadyApplied {
            current: current.clone(),
            candidate: Some(proposal.clone()),
        });
    }
    if current == base || (ops.is_ancestor(base, current)? && ops.is_ancestor(current, proposal)?) {
        return Ok(WorkspaceResolution::Direct {
            current: current.clone(),
            output: proposal.clone(),
        });
    }
    let implementation = ops.implementation();
    match ops.merge(base, current, proposal)? {
        MergeOutcome::Merged { tree } => {
            let output = ops.commit(
                &tree,
                &[current.clone(), proposal.clone()],
                RECONCILE_MESSAGE,
                signature,
            )?;
            Ok(WorkspaceResolution::Merged {
                current: current.clone(),
                merge: MergeInfo {
                    base: base.clone(),
                    ours: current.clone(),
                    theirs: proposal.clone(),
                    implementation,
                    output: Some(output.clone()),
                    conflict_paths: None,
                },
                output,
            })
        }
        MergeOutcome::Conflict { paths } => Ok(WorkspaceResolution::Conflict {
            current: Some(current.clone()),
            candidate: proposal.clone(),
            merge: Some(MergeInfo {
                base: base.clone(),
                ours: current.clone(),
                theirs: proposal.clone(),
                implementation,
                output: None,
                conflict_paths: Some(paths),
            }),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::{HashMap, HashSet};

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct MergeCall {
        base: Oid,
        ours: Oid,
        theirs: Oid,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct CommitCall {
        tree: Oid,
        parents: Vec<Oid>,
        message: String,
    }

    struct FakeOps {
        dag: HashMap<Oid, (Oid, Vec<Oid>)>,
        merge_outcome: MergeOutcome,
        merge_calls: Vec<MergeCall>,
        commit_calls: Vec<CommitCall>,
        next: u64,
        touches: Cell<usize>,
    }

    impl FakeOps {
        fn new(merge_outcome: MergeOutcome) -> FakeOps {
            FakeOps {
                dag: HashMap::new(),
                merge_outcome,
                merge_calls: Vec::new(),
                commit_calls: Vec::new(),
                next: 1,
                touches: Cell::new(0),
            }
        }

        fn add(&mut self, commit: &Oid, tree: &Oid, parents: &[&Oid]) {
            self.dag.insert(
                commit.clone(),
                (
                    tree.clone(),
                    parents.iter().map(|parent| (*parent).clone()).collect(),
                ),
            );
        }
    }

    impl CodeOps for FakeOps {
        fn is_ancestor(&self, ancestor: &Oid, descendant: &Oid) -> Result<bool, String> {
            self.touches.set(self.touches.get() + 1);
            let mut pending = vec![descendant.clone()];
            let mut seen = HashSet::new();
            while let Some(commit) = pending.pop() {
                if commit == *ancestor {
                    return Ok(true);
                }
                if seen.insert(commit.clone()) {
                    let (_, parents) = self
                        .dag
                        .get(&commit)
                        .ok_or_else(|| format!("unknown commit {commit}"))?;
                    pending.extend(parents.iter().cloned());
                }
            }
            Ok(false)
        }

        fn tree_of(&self, commit: &Oid) -> Result<Oid, String> {
            self.touches.set(self.touches.get() + 1);
            self.dag
                .get(commit)
                .map(|(tree, _)| tree.clone())
                .ok_or_else(|| format!("unknown commit {commit}"))
        }

        fn merge(&mut self, base: &Oid, ours: &Oid, theirs: &Oid) -> Result<MergeOutcome, String> {
            self.touches.set(self.touches.get() + 1);
            self.merge_calls.push(MergeCall {
                base: base.clone(),
                ours: ours.clone(),
                theirs: theirs.clone(),
            });
            Ok(self.merge_outcome.clone())
        }

        fn commit(
            &mut self,
            tree: &Oid,
            parents: &[Oid],
            message: &str,
            _signature: &Signature,
        ) -> Result<Oid, String> {
            self.touches.set(self.touches.get() + 1);
            self.commit_calls.push(CommitCall {
                tree: tree.clone(),
                parents: parents.to_vec(),
                message: message.to_string(),
            });
            let commit = Oid::parse(&format!("{:040x}", self.next), "fake commit")?;
            self.next += 1;
            self.dag
                .insert(commit.clone(), (tree.clone(), parents.to_vec()));
            Ok(commit)
        }

        fn implementation(&self) -> String {
            self.touches.set(self.touches.get() + 1);
            "fake/1".to_string()
        }
    }

    fn oid(character: char) -> Oid {
        Oid::parse(&character.to_string().repeat(40), "test oid").unwrap()
    }

    fn signature() -> Signature {
        Signature {
            name: "Test".to_string(),
            email: "test@example.com".to_string(),
            time: 1,
            offset: "+0000".to_string(),
        }
    }

    fn merged() -> MergeOutcome {
        MergeOutcome::Merged { tree: oid('f') }
    }

    #[test]
    fn already_applied_covers_base_current_and_reachable_proposals() {
        let base = oid('a');
        let proposal = oid('b');
        let current = oid('c');
        let mut ops = FakeOps::new(merged());
        ops.add(&base, &oid('1'), &[]);
        ops.add(&proposal, &oid('2'), &[&base]);
        ops.add(&current, &oid('3'), &[&proposal]);
        for (proposal, current) in [
            (&base, &proposal),
            (&proposal, &proposal),
            (&proposal, &current),
        ] {
            assert_eq!(
                reconcile(&mut ops, &base, proposal, Some(current), &signature()).unwrap(),
                WorkspaceResolution::AlreadyApplied {
                    current: current.clone(),
                    candidate: None,
                }
            );
        }
        assert!(ops.merge_calls.is_empty());
    }

    #[test]
    fn tree_equal_unreachable_proposal_is_already_applied_with_candidate() {
        let base = oid('a');
        let proposal = oid('b');
        let current = oid('c');
        let tree = oid('1');
        let mut ops = FakeOps::new(merged());
        ops.add(&base, &oid('0'), &[]);
        ops.add(&proposal, &tree, &[&base]);
        ops.add(&current, &tree, &[&base]);
        assert_eq!(
            reconcile(&mut ops, &base, &proposal, Some(&current), &signature()).unwrap(),
            WorkspaceResolution::AlreadyApplied {
                current,
                candidate: Some(proposal),
            }
        );
    }

    #[test]
    fn direct_covers_current_base_and_current_between_base_and_proposal() {
        let base = oid('a');
        let middle = oid('b');
        let proposal = oid('c');
        let mut ops = FakeOps::new(merged());
        ops.add(&base, &oid('1'), &[]);
        ops.add(&middle, &oid('2'), &[&base]);
        ops.add(&proposal, &oid('3'), &[&middle]);
        for current in [&base, &middle] {
            assert_eq!(
                reconcile(&mut ops, &base, &proposal, Some(current), &signature()).unwrap(),
                WorkspaceResolution::Direct {
                    current: current.clone(),
                    output: proposal.clone(),
                }
            );
        }
        assert!(ops.merge_calls.is_empty());
    }

    #[test]
    fn successful_merge_records_inputs_and_mints_two_parent_commit() {
        let base = oid('a');
        let ours = oid('b');
        let theirs = oid('c');
        let tree = oid('f');
        let mut ops = FakeOps::new(MergeOutcome::Merged { tree: tree.clone() });
        ops.add(&base, &oid('1'), &[]);
        ops.add(&ours, &oid('2'), &[&base]);
        ops.add(&theirs, &oid('3'), &[&base]);
        let resolution = reconcile(&mut ops, &base, &theirs, Some(&ours), &signature()).unwrap();
        let output = Oid::parse(&format!("{:040x}", 1), "output").unwrap();
        assert_eq!(
            resolution,
            WorkspaceResolution::Merged {
                current: ours.clone(),
                merge: MergeInfo {
                    base: base.clone(),
                    ours: ours.clone(),
                    theirs: theirs.clone(),
                    implementation: "fake/1".to_string(),
                    output: Some(output.clone()),
                    conflict_paths: None,
                },
                output,
            }
        );
        assert_eq!(
            ops.merge_calls,
            vec![MergeCall {
                base,
                ours: ours.clone(),
                theirs: theirs.clone(),
            }]
        );
        assert_eq!(
            ops.commit_calls,
            vec![CommitCall {
                tree,
                parents: vec![ours, theirs],
                message: RECONCILE_MESSAGE.to_string(),
            }]
        );
    }

    #[test]
    fn merge_conflict_records_sorted_paths_from_implementation() {
        let base = oid('a');
        let ours = oid('b');
        let theirs = oid('c');
        let paths = vec!["a.txt".to_string(), "b.txt".to_string()];
        let mut ops = FakeOps::new(MergeOutcome::Conflict {
            paths: paths.clone(),
        });
        ops.add(&base, &oid('1'), &[]);
        ops.add(&ours, &oid('2'), &[&base]);
        ops.add(&theirs, &oid('3'), &[&base]);
        assert_eq!(
            reconcile(&mut ops, &base, &theirs, Some(&ours), &signature()).unwrap(),
            WorkspaceResolution::Conflict {
                current: Some(ours.clone()),
                candidate: theirs.clone(),
                merge: Some(MergeInfo {
                    base,
                    ours,
                    theirs,
                    implementation: "fake/1".to_string(),
                    output: None,
                    conflict_paths: Some(paths),
                }),
            }
        );
        assert!(ops.commit_calls.is_empty());
    }

    #[test]
    fn current_behind_base_reconciles_instead_of_directly_adopting() {
        let current = oid('a');
        let base = oid('b');
        let proposal = oid('c');
        let mut ops = FakeOps::new(merged());
        ops.add(&current, &oid('1'), &[]);
        ops.add(&base, &oid('2'), &[&current]);
        ops.add(&proposal, &oid('3'), &[&base]);
        let resolution =
            reconcile(&mut ops, &base, &proposal, Some(&current), &signature()).unwrap();
        assert!(matches!(resolution, WorkspaceResolution::Merged { .. }));
        assert_eq!(ops.merge_calls.len(), 1);
    }

    #[test]
    fn absent_pointer_conflicts_without_touching_code_store() {
        let mut ops = FakeOps::new(merged());
        let base = oid('a');
        let proposal = oid('b');
        assert_eq!(
            reconcile(&mut ops, &base, &proposal, None, &signature()).unwrap(),
            WorkspaceResolution::Conflict {
                current: None,
                candidate: proposal,
                merge: None,
            }
        );
        assert_eq!(ops.touches.get(), 0);
    }

    #[test]
    fn proposal_must_descend_from_base() {
        let base = oid('a');
        let proposal = oid('b');
        let current = oid('c');
        let mut ops = FakeOps::new(merged());
        ops.add(&base, &oid('1'), &[]);
        ops.add(&proposal, &oid('2'), &[]);
        ops.add(&current, &oid('3'), &[&base]);
        assert_eq!(
            reconcile(&mut ops, &base, &proposal, Some(&current), &signature()),
            Err(format!(
                "proposal {proposal} does not descend from its base {base}"
            ))
        );
        assert!(ops.merge_calls.is_empty());
    }
}
