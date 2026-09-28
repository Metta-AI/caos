# drive

Start and inspect Claude Code cloud sessions, as a caos worker. A session runs
in a named cloud **environment**, on a named GitHub **repository**, and answers
prompts — and neither of those is a property of the directory anything is
started from.

```
caos-cli run --base:@=integrations/claude-code/drive --verb=start \
  --env=Caos --repo=Metta-AI/caos-session --prompt='the first prompt' \
  --at="$(date +%s)"
```

| | |
|---|---|
| [`.caos-expr`](.caos-expr) | **how to call it**: every verb, every argument, and why `at` is one of them. The `help` bound there is what a harness offers a model. |
| [`drive.go`](drive.go) | the worker: how it reaches the API, and why not through `claude --cloud`. Refuses to run on the host. |
| [`authorize.go`](authorize.go) | mints the token the worker reads from `/secret/claude-oauth-token`. Runs **only** on the host — it needs a browser and a paste. |
| `DEPS` | `std/go`, which supplies the interpreter and the container. |

## Setting it up

```
go run integrations/claude-code/drive/authorize.go
```

Approve in the browser, paste back the code, and put what it prints in
`.caos-secrets/claude-oauth-token` as `value=`, keeping `reader=` and
`entropy=`. **30 days is the server's ceiling on the scopes this needs**, so it
is a monthly chore; `authorize.go` explains why the shorter and longer-lived
alternatives do not work.

`go` is in the dev shell; outside it, `nix run nixpkgs#go -- run …`.

## Why it is a worker and not a command

The verbs reach the API with the token at `/secret`. Run on the host they would
reach it with whatever `~/.claude/.credentials.json` happens to hold — another
account's, a stale one, or one with the wrong scopes — so `drive.go` refuses,
and the entry is the only way in. That also means every call leaves a job
behind to look at.

The cost is that a result is memoized like any other worker's, which is what
`at` is for.
