# Shared test state

Suite runs for the same built image may reuse a persistent test stack, CAS, and
Git remote. This is independent of the outer run's `CAOS_SALT`, so mutable refs
and conversation names can collide between runs.

Generate a prefix inside each test that owns mutable names, and reuse it for all
of them:

```bash
test_run_id="$(date +%s%N)-$$-$RANDOM"
conversation="${test_run_id}-tools"
```

`CAOS_SALT` controls caching; it is not a state namespace.

The git-import suite entry runs a private HTTPS Git remote and a separate
server, checking exact-commit imports, authentication, complete history,
incremental transfers and retries. It also checks leased pushes, lost replies,
concurrent updates, code-history preservation and publication guards. It needs no external service.

The llm-import entry exercises agent-driven importing in an empty conversation,
including provenance, readable history, occupied destinations, and invalid
sources. Pinning and replay are also covered by llm-step's unit tests.

# Writing a test

A new test is a Go worker: `worker.go` curried onto `DEEP-DEPS/go`, importing
`caos/w`, staged with `curry` + `run-then` as `tests/create-squashed-stack` and
`tests/flake-update` are. The `worker.sh` tests predate `std/go`; do not copy
them for a new one.

# Disabling a test

Disable a test by making its worker return early. Do not move or delete it.

Put this straight after `set -euo pipefail`, before the test sources anything or
does any work, with a comment saying why it is disabled and what would let it
come back:

```bash
# DISABLED: <why, and what would let it come back>.
echo "<name>: SKIPPED (disabled)" > /tmp/report
caos put /tmp/report /cas/out
exit 0
```

The suite runs every directory under `tests/` (`dev/run-tests/worker.sh`), and a
disabled test stays one of them, so:

- It is still listed in the report, still evaluated, and its `DEPS` still
  resolve. A broken `.caos-expr` or a dependency that has gone away is still
  caught. A test moved out of `tests/` drops out of the report unannounced, and
  nothing notices it rotting.
- It reports as passed (✓), because the worker finishes normally. Its narration
  says `SKIPPED`: `caos-std/caos-test-result` with the hash from the report prints
  it.
- Re-enabling it is deleting those lines.
