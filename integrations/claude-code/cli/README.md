# cli — launch Claude Code locally against caos

Run Claude Code on your own machine with caos's tools instead of its built-ins.
Needs a `caos` you already have (`nix build`, or `CAOS_BIN=…`) and a `caos` git
remote pointing at a server.

```
run              a normal interactive session
remote-control   `claude remote-control`, driven from claude.ai/code or the app
```

Both pass `../shared/settings.json` (the deny list + hooks) and
`../shared/mcp.json` (the `caos mcp serve` declaration) EXPLICITLY, so nothing is
written into your checkout, and both probe the server first so a stale binary
fails here rather than leaving a session quietly toolless.

Neither uses `cloud/install.sh` — that provisions a container from a release; on
a machine you already build caos on, there is nothing to install.
