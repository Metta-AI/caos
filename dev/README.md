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

A worked pass from an agent session, with a checkout of caos imported into the
conversation (`import_source`, then `copy` to `feature/...`):

- Reach the tools BY PATH in that checkout: `feature/<name>/dev/caos-stack` and
  `feature/<name>/integrations/claude-code/drive`, with `run_tool`. (`caos-std/`
  is the std pinned by the client repo, not the tree under test, and no longer
  holds `caos-stack`.)
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
- Edit the checkout AFTER you are done driving. `drive` is granted its token only
  while its tree is unedited, so once you edit `feature/<name>` every `drive` call
  there fails with "no token at /secret/claude-oauth-token" (see AGENTS.md,
  "Resident workers"). A stack is also keyed by tree hash, so `stop` from the
  edited tree addresses a different stack. Keep an untouched import
  (`imports/<repo>/base`) and run `drive` and `stop` from it: it is the same
  code, and it still has the grant.
- Not understood yet: `harvest` returned `harvested=0` and `drive conv` found no
  head on the host server, even after a session had completed turns against the
  stack. Do not rely on either to retrieve the conversation until that is
  explained.
- Clean up: `drive archive`, `drive env-delete`, then `caos-stack stop`.
