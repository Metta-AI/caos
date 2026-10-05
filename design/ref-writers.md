# Ref writers

**Status:** proposed. Implemented by the stack built on this change.

Before this, any worker could push any ref. This is how caos controls who can
write a ref.

## Goals

- Allow multiple writers to a ref
- Allow removal of writers
- Add or remove access to many related refs at once, rather than one change per
  ref. This matters because a single caos agent conversation can spin up many
  other refs, like ones for its subagents
- Allow any reader
- caos jobs with write access can pass it on to the jobs/runs they create
- Works with caos jobs spun up from a claude cloud session

## Keys

Each writer has an ed25519 ref writer key. `caos-cli ref-writer-key new`
prints a private key (and the public one on stderr); the private key stays on
the writer's device, in the checkout's git config as `caos.ref-writer-key`,
next to `caos.secret-readers`. `caos-cli ref-writer-key show` prints the public
key.

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
9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60  nishad
```

A `.caos/writers` file anywhere else means nothing. Governed refs can point at
anything (a source branch shouldn't have to carry the file).

To create a namespace, make a root commit whose `.caos/writers` lists yourself,
and push it to `refs/caos/w/<that commit's hash>/writers`. Because the id is
the hash of the initial list, nobody can claim a namespace ahead of you or
create one you aren't in. The first commit is deterministic (author and
committer `caos <caos>` at time 0, message `caos namespace\n\n<label>`), so the
same writers and label always name the same namespace; `caos-cli namespace new
[<label>]` makes one.

One list governs every ref in the namespace, e.g. a conversation's head and its
subagents' heads. Removing someone removes them from all of them at once, and a
job can create new refs in the namespace without setting up their writers
first.

To add or remove a writer, push a new commit on `writers` (`caos-cli writers
add|remove <namespace|conversation> <key> [<label>]`). It must be signed by
someone in the current list, and must be a fast-forward, so the history of who
added whom is kept. Removal takes effect on the next push.

Only a writer key can change `writers`; a job's run token (below) can't. Jobs
write content, writers decide who else writes. A job may found a new namespace
that lists only its own writer.

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
  replayable while the ref is at `old` again. The expiry (ten minutes) covers
  that case.

`caos-cli ref-push <rev> <ref>` pushes one ref, signed.

**Jobs present a run token**:

```text
caos-auth=run:<token>
```

Jobs never hold keys. When the server dispatches a job with write access, it
mints a token, records `token -> (writer pubkey, namespaces)`, and injects it at
`/secret/caos-write`. The token dies when the job's container is done. The hook
resolves a token to its pubkey and checks that against `writers`, so removing
someone also cuts off their running jobs. A worker's scratch `GitStore` picks
the token up on its own.

## Which jobs get a token

A job asks for write access in its ArgTree, and the server grants it out of
band:

- A job asks with a `writes` arg: namespace ids, or `*` for everything its
  creator was handed. No `writes`, no token, so e.g. a shell tool never gets
  one.
- A top-level request carries `X-Caos-Write: <pubkey> <expiry> <signature>`,
  signed by the client over the expiry and the request's method and target. It
  hands the job everything its writer may write.
- A job's token covers `writes ∩ available`, where `available` is the header's
  writer for a top-level request, and **what its creator was granted** for a
  continuation or child.

So a job can pass on only what it holds. `llm-step` holding a conversation can
put it in `writes` for `run-and-update-ref` and child `llm-step`s; a shell it
starts gets nothing, and so does anything that shell starts. This rides
alongside `secrets::Context`, which is already threaded to sub-runs.

A job that only passes writes on asks for `*`. The suite does this: the client
driving it signs as a fresh writer, and `dev/run-tests`, `dev/run-test` and
each test hold `writes=*`, so a test's own steps can be handed their
conversation's namespace.

A cache hit runs nothing, so it writes nothing. A request without access for an
uncached job runs without a token, its push is refused and the job fails.

## The hook

The server installs a pre-receive hook (`<git dir>/hooks/pre-receive`) that
re-execs the server binary. Being a repo hook, it covers both smart HTTP and
iroh. The server's own ref writes (`refs/caos/res/*` etc.) don't go through
`receive-pack`, so it never sees them.

| Ref | Accepted when |
| --- | --- |
| content-named: `refs/caos/req/<h>`, `refs/heads/caos-test/<h>` | it points at `<h>`, or is deleted |
| unguarded (`caos.unguardedRef`; `refs/caos/dev` by default) | always |
| `refs/caos/w/<id>/writers`, create | `new == <id>`, `new` has no parents, pusher is in `new`'s list |
| `refs/caos/w/<id>/writers`, update | signer is in `old`'s list, fast-forward |
| `refs/caos/w/<id>/<other>` | signer's (or token's) key is in the current `writers`; a token must cover `<id>` |
| anything else | rejected |

Cost is one blob read per namespace touched. Needs
`receive.advertisePushOptions=true`, which the server sets.

`git config caos.refWriters report` on the server's repository logs what the
hook would refuse and accepts it, for rolling out to a server with writers this
table missed.

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

A conversation lives in a namespace:

```text
refs/caos/w/<ns>/writers
refs/caos/w/<ns>/conversations/<hex id>/head
refs/caos/w/<ns>/conversations/<hex child id>/head    a subagent
```

- `<ns>` is fixed by the creator's key and the conversation id (writers: the
  creator; label: `conversation <hex id>`), so a client finds its own
  conversation without a lookup. Another writer's is found by listing
  `refs/caos/w/*/conversations/<hex id>/head`.
- Conversation ids are unchanged (`cc/<session>`, `talk-1`, …). Across a
  process boundary a conversation is named by its address, `<ns>/<id>`:
  `llm-step`'s `--conversation`, the `X-Caos-Conversation` header, and a
  secret's `reader:@=<path> conversation=<address>`. An id alone would match a
  conversation of that name in anyone's namespace.
- A subagent's head sits in its parent's namespace. `llm-step` keeps its own
  `writes` on the child's request and puts the namespace on the relay's, so no
  keys are minted at runtime.
- The sidebar is `refs/caos/w/<personal>/memberships/{active,archived}/<ns>/<hex
  id>`, in a personal namespace fixed by the key (label `personal`).
- `caos-cli conversation-ref <id|address>` prints a conversation's head ref.

Multiplayer:

1. Nishad sends Malcolm their public key (it isn't secret)
2. Malcolm adds it: `caos-cli writers add <conversation> <key>`, or `/invite
   <key>` in the tui
3. Nishad opens the conversation by its address from their own tui or session

Each writer's pushes and jobs use their own key, so the log shows who drove
each turn.

## Migration

| Pushed before | Now |
| --- | --- |
| `refs/caos/req/<h>` | unchanged, content-named |
| `refs/heads/caos-test/<sha>` | unchanged, content-named |
| `refs/caos/v3/conversations/…`, `refs/caos/v3/users/…` | namespaces, as above |
| `refs/caos/dev` | unguarded by default |
| tests pushing `refs/heads/*` | `caos-cli namespace new` + `ref-push`, or a content-named ref |

Conversations under `refs/caos/v3/` stay in the repository, unlisted; nothing
moves them.

## Alternatives considered

- **One keypair per ref, shared as an invite.** Removing someone means moving
  everyone to a new ref, you can't tell writers apart, and a cloud session
  can't hold a key per conversation.
- **Writer private keys in the secret store, for jobs to sign with.** Gives the
  server everyone's keys, and grants by image, so it can't express "pass this
  on to my children".
- **The conversation id as the namespace id.** Subagents would need namespaces
  of their own, and `cc/<session>` would need a lookup in every hook process.

## Open questions

- `refs/caos/dev` is unguarded: any job can rewrite what dev-mode bootstrap
  installs. An operator namespace whose id bootstrap is given would close it.
- A signature can be replayed on another server holding the same namespace
  until it expires. Adding the server's identity to the signed text would close
  that, if it matters.
- `writes=*` is broad by design. A job that holds it can write every namespace
  its writer may.
