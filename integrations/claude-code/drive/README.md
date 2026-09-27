# drive

Start and inspect Claude Code cloud sessions, as a caos worker.

```
caos-cli run --base:@=integrations/claude-code/drive --verb=start \
  --env=Caos --repo=Metta-AI/caos-session --prompt='the first prompt' \
  --at="$(date +%s)"
```

```
drive.go       the worker. Refuses to run on the host: the verbs reach the API
               with the token at /secret, and a host run would reach it with
               whatever login the machine happens to hold.
authorize.go   mints that token. Runs ONLY on the host — it needs a browser
               and a paste, and a worker has neither.
.caos-expr     makes this directory an entry; DEPS names std/go.
```

Set the secret up once with `go run integrations/claude-code/drive/authorize.go`
(see *The credential* below). `go` is in the dev shell; outside it,
`nix run nixpkgs#go -- run …`.

The verbs are `start send list info conv archive env-list env-show env-update
env-create env-delete`, each a mode of one command line — `drive.go --help`
lists them with their arguments.

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

## The credential

An Anthropic OAuth token carrying `user:sessions:claude_code` runs all of it —
sessions and environments alike, including an environment's whole definition.
Measured: those two scopes alone answer 200 on both route families.

```
go run integrations/claude-code/drive/authorize.go
```

It prints a claude.ai URL, you approve in a browser and paste back the code it
shows, and it prints a token for `.caos-secrets/claude-oauth-token`. The
consent page says **"Claude Code"** is asking: that is the OAuth client's
registration, held by Anthropic, and not a parameter of the request — nothing
sent from here can change it.

**30 days is the ceiling.** 60 and above are refused as `Invalid expiry for
scope`, so this is a monthly chore, not a one-off. Re-run it and replace
`value=`; `reader=` and `entropy=` stay.

Three routes that look like they should work and do not:

| | |
|---|---|
| `claude setup-token` | consents at the CONSOLE, so the grant is the console scopes; every route answers `401 oauth_scope_insufficient` |
| a `refresh_token` exchange | right scopes, but `expires_in` is ignored there (8 hours) and the refresh token is SINGLE-USE — only something that can persist the replacement can spend it, and a worker cannot |
| a browser `sessionKey` | a cookie, not a bearer; it reaches claude.ai's environment-definition service and nothing else |

An API key (`sk-ant-api03-…`) is refused outright: "Cloud sessions are only
available on the first-party Anthropic API provider".

At run time the token is looked for in `$CLAUDE_CODE_OAUTH_TOKEN`, then
`/secret/claude-oauth-token`, then `~/.claude/.credentials.json`. A worker only
ever has the second.
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
from `/secret/claude-oauth-token`, and it reports what it printed.

The secret is granted to a job whose ArgTree is a superset of one of the
secret's readers, so `.caos-secrets/claude-oauth-token` needs:

```
value=<what authorize.go prints>
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
