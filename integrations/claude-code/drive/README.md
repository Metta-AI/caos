# drive

Start and inspect Claude Code cloud sessions. One program, two ways to run it:

```
go run integrations/claude-code/drive/drive.go \
  --env Caos --repo Metta-AI/caos-session 'the first prompt'

caos-cli run --base:@=integrations/claude-code/drive --verb=start \
  --env=Caos --repo=Metta-AI/caos-session --prompt='the first prompt' \
  --at="$(date +%s)"
```

`go run` needs `go`, which the dev shell carries; outside it,
`nix run nixpkgs#go -- run …`. `--help` lists every mode.

## A session names its environment and its repository

Neither comes from the directory this is started in. `--env` takes an
environment's name or its `env_…` id (`--env-config` lists them); `--repo`
takes `owner/repo`, `owner/repo@ref`, or a URL. An omitted ref leaves the
branch to the server rather than guessing a name for the default. Omitting
`--repo` falls back to the cwd's `origin`, and omitting `--env` to the
`remote.defaultEnvironmentId` settings key or the account's first hosted
environment.

**`claude --cloud` can express neither**, which is why the create goes to the
API under it. Its `--environment` flag takes only a self-hosted `ccpool_…` id —
a hosted environment is chosen by a settings key, one per machine — and it has
no repository flag at all: the repository is `git remote get-url origin` in the
current directory, full stop. So the tree you are working in decides what the
session opens, and for caos that is the one repository it must not be: caos is
not a client repo, so its setup script fails, four minutes and one container
later. `POST /v1/code/sessions` takes `environment_id` and a `git_repository`
source directly.

**A session id has two spellings.** The API and this print `cse_<suffix>`; a
claude.ai/code URL carries `session_<suffix>`. Both resolve on the API, and
every mode takes either, or the URL.

## One credential

An Anthropic OAuth token with the `user:sessions:claude_code` scope runs all of
it — sessions and environments alike, including an environment's whole
definition. It is looked for in three places, first hit wins:

| | |
|---|---|
| `$CLAUDE_CODE_OAUTH_TOKEN` | a token you supply |
| `/secret/claude-oauth-token` | the caos secret, dropped by the runner |
| `~/.claude/.credentials.json` | the running CLI's own login — it EXPIRES |

**`claude setup-token` does not mint a usable one.** Its flow asks for the
console scopes, and every session and environment route answers a token
without `user:sessions:claude_code` with `401 oauth_scope_insufficient`. What
carries the scope is the CLI's own claude.ai login, which is short-lived —
so `drive --mint` trades that login's refresh token for a long-lived access
token with the same scopes, and writes back a rotated refresh token so the
CLI keeps working.

An API key (`sk-ant-api03-…`) is **not** one of these: the service answers
"Cloud sessions are only available on the first-party Anthropic API provider".
Nothing here needs the claude.ai `sessionKey` cookie, and nothing shells out to
`claude`.

The routes, all `Authorization: Bearer <token>` and
`anthropic-version: 2023-06-01`, with `x-organization-uuid` optional:

```
POST /v1/code/sessions                      create (environment_id + config.sources)
POST /v1/code/sessions/<id>/events          send a prompt
GET  /v1/code/sessions[?limit=]             list
GET  /v1/code/sessions/<id>[/events]        read
POST /v1/code/sessions/<id>/archive         end it
GET  /v1/environment_providers              list environments
GET  /v1/environment_providers/<id>         the whole definition
POST /v1/environment_providers/cloud/create create one
POST /v1/environment_providers/<id>         replace name/description/config
POST /v1/environment_providers/<id>/delete  delete one
```

## As a worker

`.caos-expr` and `DEPS` make this directory an entry: `std/go` `go run`s
`drive.go` as `/worker`, it reads its arguments from `/cas/args` and its token
from `/secret/claude-oauth-token`, and it reports what it printed. The verbs
are `start send list info conv archive env-list env-show env-update env-create
env-delete`, each one a mode of the same command line.

The secret is granted to a job whose ArgTree is a superset of one of the
secret's readers, so `.caos-secrets/claude-oauth-token` needs:

```
value=<what `drive --mint` prints>
reader=integrations/claude-code/drive
entropy=<`caos secrets` fills this>
```

**Every verb requires `at`, the current time.** A worker's result is memoized
on its ArgTree, and nothing here is a pure function of its arguments: `start`
and `send` change a session, and `list`, `info` and the env reads ask a service
that moves on its own. Without a value that moves, a second `start` answers
with the first session's id and creates nothing — the session looks started
and no container exists — and a second `list` answers with a listing from
whenever the first one ran.

A time rather than a nonce, so staleness is the caller's to choose:
`date +%s` is always fresh, `date +%Y%m%d%H%M` reuses an answer for up to a
minute, and a fixed value deliberately pins one. Refused when missing rather
than defaulted — a default that varies takes that choice away, and one that
does not vary fixes nothing.

**The read verbs are account-scoped**, which a cached result may only be
because the secret's entropy pins that identity in `secret-hash` (SPEC.md,
*Correctness requirements*). Rotate the entropy if the token starts naming a
different account.
