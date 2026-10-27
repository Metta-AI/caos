# Disabled tests

The suite runs every directory under `tests/` (`dev/run-tests/worker.sh`), so a
test is disabled by moving it here. Both directories sit at the same depth as
`tests/<name>`, so their `DEPS` (`../../std/...`) still resolve. To re-enable
one, move it back to `tests/<name>`.

- `caos-tools`: flaky under load. Its single turn makes about 19 tool calls, each
  its own sub-run, and `wait_turn` (`std/llm-test/worker-common.sh`) gives up
  after 120s (`wait-terminal` exits 2). It took 130s when it failed on CI.
- `llm-subagent-path`: flaky under load. The parent and child agents race for
  the stub's next reply, so the test answers each request by hand and polls for
  `request-N.json` with `wait_for_file`, which gives up after 60s per request.
  A request that never arrives (`parent/child API request N never arrived`)
  means the sub-runs were not scheduled in time.

Both are to be made deterministic (replies scripted up front, no polling)
before they come back.
