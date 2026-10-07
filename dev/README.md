# dev

caos's own development tooling: the dev container image, the test-stack daemon
and the tools that drive it. (Harness glue is under `integrations/`, and the
tools a model calls are under `std/`.)

| entry | what it is |
|---|---|
| `devbox/` | The container image the suite and the stacks run on: nix, podman, git. |
| `test-stack/` | The daemon that builds, tests and runs a stack from a tree (`design/daemons.md`). |
| `caos-stack/` | The tool that sends the daemon `start`, `status`, `logs`, `harvest` and `stop`; `start` with `cloud-env` also makes a cloud environment pointed at the stack. |
| `remove-dev-envs/` | Deletes the cloud environments `caos-stack start` made. |

## Driving a cloud session at a test stack

A worked pass from an agent session. Import caos's `main` once, UNEDITED, as
`imports/caos/main` (`import_source` with `source=https://github.com/Metta-AI/caos`,
`into=imports/caos/main`). That import is where you RUN the tools from. If you
are changing caos, `copy` it to `feature/<name>` and edit THAT: it is the tree
under test, not the place the tools run.

- Reach the tools BY PATH in the import: `imports/caos/main/dev/caos-stack` and
  `imports/caos/main/integrations/claude-code/drive`, with `run_tool`. (`caos-std/`
  is the std pinned by the client repo, not the tree under test, and no longer
  holds `caos-stack`.) Pass the tree to bring up as `in`: `caos-stack` takes it
  (`tool_help` lists it), e.g. `in=feature/<name>`. Leave `in` off to test the
  import itself.
- Do NOT run the tools from `feature/<name>`. `drive` is granted its token only
  when the root tree it is run from is one `caosd up` published (AGENTS.md,
  "Resident workers"), so an edited checkout can fail every `drive` call with
  "no token at /secret/claude-oauth-token". One did, after `drive` itself was
  edited; whether an edit elsewhere also breaks it has not been tested. The
  untouched import is the blessed copy, so it avoids the question.
- `caos-stack` `op=start` needs `relay=http://<ip>/`. Your own session already
  has one: `caos_status` shows `over RELAY relay:http://<ip>/` on its `push:`
  lines. Use `cloud-env=1` to get the environment, and a fresh `request-id` for
  every call.
- `drive` `verb=start` with `env=z Caos Dev <commit>`, `repo=<owner>/<repo>`
  and a `prompt`. `env` is the name `start` printed (`cloud_env=`); `repo` is
  any repository the cloud session can clone (this session's own repo works;
  the stack does not care which). `at` is the epoch seconds now, and must be
  different on every `drive` call: a repeated value returns the earlier answer. Provisioning takes about
  two minutes; read the result with `drive info` (its `summary` is the turn's
  outcome, empty while the turn runs).
- Each stack start mints a new ticket and dev commit, and the stack is evicted
  when its slot is wanted, so keep one `status` going between turns; after an
  eviction `start` again WITH `cloud-env` and start a new session on the new
  environment. An old environment cannot be repointed.
- A stack is keyed by the hash of the tree under test, so editing
  `feature/<name>` while a stack is up makes `stop` and `status` address a
  different stack. `stop` before editing, or keep the tree you started with
  untouched.
- `drive info` is how you read a turn, and the only way that works today. Poll
  it with a new `at` each time (a repeated `at` returns the cached answer).
  Until the session's container finishes setup (about 2 minutes) it shows only
  the provisioning trace; `worker tools:` appears once Claude Code is up; the
  `summary:` is empty while a turn runs and filled in when it ends. Expect
  about 2 minutes for a first turn and about 1 minute for a `send` that makes
  a couple of caos calls. The summary is the session's own account of what it
  did (a worked pass: asking it to `write` hello.txt and `read` it back
  produced `wrote hello.txt (17 bytes) and read it back`).
- A session started with a plain greeting only lists its tools; give it a
  task that calls a caos tool if you want to see the stack do work (the
  stack's `logs` with `log=server` then shows `cache miss` / `ran worker`).
- `drive conv` and `harvest` do not retrieve the conversation. `conv` looks
  for the session's head on YOUR session's server (the `http://10.x.x.x` in its
  error), not on the test stack, so it reports "not on ... wrong server, or no
  turn yet" even after turns completed; it also prints the transcript of an
  unrelated conversation before failing, which is not the session's. `harvest`
  returned `harvested=0` with the default `refs/caos/v3/conversations/*` after
  completed turns; why is still not understood. Use `drive info`'s summary.
- The stack can be gone within minutes of `start` (a `stop` ten minutes later
  said `not running`). That is the eviction above, not a failed start.
  `stop` then has nothing to do; carry on with `archive` and `env-delete`.
- Clean up: `drive archive`, `drive env-delete`, then `caos-stack stop`.
