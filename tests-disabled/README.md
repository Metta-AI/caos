# Disabled tests

The suite runs every directory under `tests/` (`dev/run-tests/worker.sh`), so a
test is disabled by moving it here. Each directory sits at the same depth as
`tests/<name>`, so its `DEPS` (`../../std/...`) still resolve. To re-enable one,
move it back to `tests/<name>`.

- `caos-tools`: flaky under load. Its single turn makes about 19 tool calls, each
  its own sub-run, and `wait_turn` (`std/llm-test/worker-common.sh`) gives up
  after 120s (`wait-terminal` exits 2). It took 130s when it failed on CI. Its
  replies are already scripted up front, so the fix is a longer deadline
  (`wait_turn 600`, as `llm-import` uses), not a change of design.
