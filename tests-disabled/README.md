# Disabled tests

Tests here are not run: the suite runs the directories under `tests/`, and
nothing else. To restore one, `git mv tests-disabled/<name> tests/<name>`. Keep it
at this depth, so its `DEPS` paths (`../../std/...`) stay valid.

## git-import

Disabled after #346, which made the server's startup repack and object-closure
walk run only when `CAOS_STORE_CHECK=yes` (`caosd` sets it; test stacks don't).
The fixture relied on that walk as a side effect, in two places:

1. `git-commands` is created by the `git` shim the fixture puts on the SERVER's
   `PATH`, and only when the server runs `rev-list`, `cat-file` and the like. The
   startup walk was the only thing that had run git by the time the fixture first
   reads the file (`fixture.py`, `commands = git_commands.read_text()`), so it now
   dies with `FileNotFoundError: .../git-commands`. The fixture should create the
   file itself.
2. The startup-integrity section at the end writes a broken object into the
   server's store, starts the server and expects "restore the missing objects".
   That check now needs `CAOS_STORE_CHECK=yes`, and with it the startup repack
   packs the store, so the `(odb/objects/..).unlink()` that follows has no loose
   file to remove. Each refusal scenario should run on a fresh copy of the store.
