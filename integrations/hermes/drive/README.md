# drive

> **First cut.** This drives a gateway over its `/api/ws` socket from a script (Python,
> because that is what Hermes's own client is written in). It is meant to be replaced by a
> caos tool that runs `hermes chat` against the gateway, reachable from a conversation.

Talk to a Hermes gateway from a terminal: start a chat session, send it a
prompt, read the reply and the tools it ran, resume it later. It speaks the same
`/api/ws` JSON-RPC the Desktop app does.

```
integrations/hermes/drive/drive --ticket "$(cat <state>/iroh-hermes/ticket)" \
  send --provider anthropic --model claude-sonnet-5-5 'the first prompt'
integrations/hermes/drive/drive --ticket "$T" send --session <id> 'a follow-up'
integrations/hermes/drive/drive --ticket "$T" list
integrations/hermes/drive/drive --ticket "$T" history <id>
```

`send` prints the reply on stdout, tool calls and the stored session id on
stderr, and exits 1 if the turn errored. `--url http://host:port` replaces
`--ticket` for a gateway you can already reach. `--token` defaults to `caos`,
the session token `stack/serve` pins.

Unlike the Claude Code `drive` this is a plain script, not a worker: the gateway
is reached over an iroh ticket with `dumbpipe`, which the script fetches with
`nix shell`.

**The gateway needs its own model credential.** The key is not part of a
session. It lives in the gateway's `.env`, which is what Desktop's Keys UI
writes through `PUT /api/env`. Without one a turn fails with "not logged into
Nous Portal". The gateway's default model slug belongs to Nous, so a
direct-Anthropic key also needs `--provider anthropic --model <id>` on `send`
(or the gateway's config changed).
