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
  and a `prompt`. `at` is any integer not used before. Provisioning takes about
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
- Not understood yet: `harvest` returned `harvested=0` and `drive conv` found no
  head on the host server, even after a session had completed turns against the
  stack. Do not rely on either to retrieve the conversation until that is
  explained.
- Clean up: `drive archive`, `drive env-delete`, then `caos-stack stop`.
