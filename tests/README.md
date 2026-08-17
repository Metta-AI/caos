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
