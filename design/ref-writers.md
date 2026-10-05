# Ref writers

Right now any worker can push any ref. This proposes a way to control who can
write a ref.

## Goals

- Allow multiple writers to a ref
- Allow removal of writers
- Allow any reader
- caos jobs with write access can pass it on to the jobs/runs they create
- Works with caos jobs spun up from a claude cloud session

## Keys

Each writer has an ed25519 ref writer key, generated with
`caos-cli ref-writer-key new`. The private key stays on the writer's device, in
the checkout's git config as `caos.ref-writer-key`, next to
`caos.secret-readers`.

## Namespaces

A namespace is a group of refs that share one list of writers. Its name, the
`<id>` below, is not chosen: it's the hash of the commit that created it.

```text
refs/caos/w/<id>/writers      the namespace's writers list
refs/caos/w/<id>/<anything>   the refs it governs
```

`writers` is one specific ref. Each commit on it has a tree with a single file,
`.caos/writers`, and the latest commit is the current list:

```text
# <ed25519 pubkey, hex>  <label, display only>
3b6a27bcceb6a42d62a3a8d02a6f0d73653215771de243a63ac048a18b59da29  malcolm
9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60  nishu
```

A `.caos/writers` file anywhere else means nothing. Governed refs can point at
anything (a source branch shouldn't have to carry the file).

To create a namespace, make a root commit whose `.caos/writers` lists yourself,
and push it to `refs/caos/w/<that commit's hash>/writers`. Because the id is
the hash of the initial list, nobody can claim a namespace ahead of you or
create one you aren't in.

One list governs every ref in the namespace, e.g. a conversation's head and its
subagents' heads. Removing someone removes them from all of them at once, and a
job can create new refs in the namespace without setting up their writers
first.

To add or remove a writer, push a new commit on `writers`. It must be signed by
someone in the current list, and must be a fast-forward, so the history of who
added whom is kept. Removal takes effect on the next push.

Only a writer key can change `writers`; a job's run token (below) can't. Jobs
write content, writers decide who else writes.

## Proving a write

Every push to a governed ref carries a push option, `git push -o caos-auth=…`.

**Clients sign the ref update**:

```text
caos-auth=sig:<pubkey>:<expiry>:<signature>
```

The signature covers the expiry and each `<old> <new> <ref>`, sorted by ref.

- We sign ref moves, not commits. A signed commit doesn't stop someone from
  moving the ref back to an older signed commit.
- Since pushes already use `--force-with-lease`, a captured signature is only
  replayable while the ref is at `old` again. The expiry (a few minutes) covers
  that case.

**Jobs present a run token**:

```text
caos-auth=run:<token>
```

Jobs never hold keys. When the server dispatches a job with write access, it
mints a token, records `token -> (writer pubkey, namespaces)`, and injects it at
`/secret/caos-write`. The token dies with the run. The hook resolves a token to
its pubkey and checks that against `writers`, so removing someone also cuts off
their running jobs.

## Which jobs get a token

A job asks for write access in its ArgTree, and the server grants it out of
band:

- A job asks with a `writes` arg: a list of namespace ids. No `writes`, no
  token, so e.g. a shell tool never gets one.
- A top-level request carries
  `X-Caos-Write: <pubkey> <expiry> <namespaces> <signature>`, signed by the
  client over the request body, expiry and namespaces.
- A job's token covers `writes ∩ available`, where `available` is the header's
  namespaces for a top-level request, and **what its creator was granted** for a
  continuation or child.

So a job can pass on only what it holds. `llm-step` holding a conversation can
put it in `writes` for `run-and-update-ref` and child `llm-step`s; a shell it
starts gets nothing, and so does anything that shell starts. This rides
alongside `secrets::Context`, which is already threaded to sub-runs.

A cache hit runs nothing, so it writes nothing. A request without access for an
uncached job runs without a token, its push is refused and the job fails.

## The hook

A pre-receive hook checks every push. Being a repo hook, it covers both smart
HTTP and iroh. The server's own ref writes (`refs/caos/res/*` etc.) don't go
through `receive-pack`, so it never sees them.

| Ref | Accepted when |
| --- | --- |
| content-named: `refs/caos/req/<h>`, `refs/heads/caos-test/<h>` | it points at `<h>` |
| `refs/caos/w/<id>/writers`, create | `new == <id>`, `new` has no parents, signer is in `new`'s list |
| `refs/caos/w/<id>/writers`, update | signer is in `old`'s list, fast-forward |
| `refs/caos/w/<id>/<other>` | signer's (or token's) key is in the current `writers`; a token must include `<id>` |
| anything else | rejected |

Cost is one blob read per namespace touched. Needs
`receive.advertisePushOptions=true`.

## Claude cloud

- Add `--ref-writer-key=<key>` to the setup line, next to `--secret-readers`.
  Bootstrap writes it to the checkout's git config. It's a setup argument
  rather than an environment variable for the same reason `--server` is: the
  setup phase can't see the environment's variables
  (integrations/claude-code/cloud/README.md).
- `caos mcp` signs its own pushes with it, and sends `X-Caos-Write` on the
  requests it makes. Jobs those requests start get their access from that.
- Nothing is stored per conversation, so a fresh container is fine.

## Conversations

A conversation is a namespace:

```text
refs/caos/w/<id>/writers
refs/caos/w/<id>/head
refs/caos/w/<id>/children/<child>/head    subagents
```

replacing `refs/caos/v3/conversations/<hex id>/head`. The conversation id is
the namespace id; today's session-derived name (`cc/<session>`) moves into
`.caos/` metadata.

Multiplayer:

1. Nishu sends Malcolm their public key (it isn't secret)
2. Malcolm adds it to `writers`, from the tui or a `caos mcp` tool
3. Nishu resumes the conversation by id from their own tui or cloud session

Each writer's pushes and jobs use their own key, so the log shows who drove
each turn.

Subagents live in the parent's namespace: `llm-step` puts the namespace in each
child's `writes`. No keys are minted at runtime.

The per-user sidebar refs (`refs/caos/v3/users/<user>/…`) move to a personal
namespace whose first `writers` commit is fixed (one key, no parent, author and
committer `caos <caos>` at time 0), so anyone can compute its id from a public
key.

## Migration

| Pushed today | Becomes |
| --- | --- |
| `refs/caos/req/<h>` | unchanged, content-named |
| `refs/heads/caos-test/<sha>` | unchanged, content-named |
| `refs/caos/v3/conversations/…`, `refs/caos/v3/users/…` | namespaces, as above |
| `refs/caos/dev` | open question |
| tests pushing `refs/heads/*` | a test helper that makes a namespace with a throwaway key |

Roll out with the hook in report-only mode first, so one suite run lists any
writer missing from this table, then enforce.

## Alternatives considered

- **One keypair per ref, shared as an invite.** Removing someone means moving
  everyone to a new ref, you can't tell writers apart, and a cloud session
  can't hold a key per conversation.
- **Writer private keys in the secret store, for jobs to sign with.** Gives the
  server everyone's keys, and grants by image, so it can't express "pass this
  on to my children".

## Open questions

- `refs/caos/dev` is pushed by an operator and read by name in cloud bootstrap.
  Either an operator namespace whose id bootstrap is given, or a config list
  of unguarded refs (each one rewritable by any job).
- A detached child can outlive its creator. Its token should probably die with
  its own run.
- A signature can be replayed on another server holding the same namespace
  until it expires. Adding the server's identity to the signed text would close
  that, if it matters.
