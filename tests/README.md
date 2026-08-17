# Shared test state

Tests for the same built image may share a persistent stack, CAS, and Git
remote across suite runs, even when their `CAOS_SALT` values differ. Mutable
refs and conversation names can therefore collide between runs.

Prefix every test-owned mutable name with the execution's `CAOS_TEST_RUN_ID`:

```bash
conversation="${CAOS_TEST_RUN_ID}-tools"
```

`CAOS_SALT` controls caching; it is not a state namespace.
