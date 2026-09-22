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
