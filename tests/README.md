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

For deterministic endpoint and credential coverage, run
`python3 dev/test-remote-import.py /path/to/server [/path/to/caos]` in an empty
test container with Git and openssl. The optional worker CLI test owns `/cas`.
The fixture serves HTTPS Git locally, interrupts transfers, restarts the server,
and checks pinned replay, object visibility and credential isolation. Reserve
its ports first (defaults 9093 and 5003, overridable in the script).
