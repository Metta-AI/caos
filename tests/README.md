# Test isolation on the shared stack

Integration tests are cached jobs, but tests that actually execute for the
same built image share one persistent test stack. That includes the stack's
CAS/Git object database and its Git remote: two test runs can reach the same
repository and mutable refs even when they have different `CAOS_SALT` values.
The stack is keyed by image identity, not by salt or suite invocation.

Sharing content-addressed state is intentional. Objects and refs whose names
are their object or request hashes can safely cross runs. Mutable names are
different: conversation heads, branch-like publishing anchors, and any other
ref chosen by a test can collide with an earlier or concurrent execution.

`tests/lib/run-test.sh` therefore exports `CAOS_TEST_RUN_ID` before `cli.sh`
runs. Its shape is:

```text
<nanosecond time>-<test name>-<pid>-<random>
```

The ID is generated only after the per-test job has missed cache and started,
so it does not enter the ArgTree or make cached tests rerun. Every
non-content-addressed ref or application name that a test creates MUST begin
with this ID. For example:

```bash
conversation="${CAOS_TEST_RUN_ID}-tools"
anchor="refs/heads/${CAOS_TEST_RUN_ID}-fixture"
```

Do not use `CAOS_SALT` as a run ID or namespace. Salt controls cache identity;
it may be absent, repeated, or different between two clients of the same
stack. It remains inherited across the nested test boundary, and removing it
from the environment is forbidden. Do not delete or force-update a fixed ref
as cleanup either: concurrent test runs make that another cross-run race.

When a test intentionally coordinates through a shared mutable ref, document
the ownership and concurrency protocol next to that ref. Otherwise the
execution prefix is required.
