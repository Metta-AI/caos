# Ref writers

**Status:** proposed; the stack on top of this change implements an earlier
draft.

Before this, any worker could push any ref. This is how caos decides who can
write one.

## Goals

- Multiple writers per ref
- Writers can be removed
- Add or remove access to many related refs in one change, not one per ref. A
  single caos agent conversation can spin up lots of other refs, like ones for
  its subagents
- Anyone can read
- A caos job with write access can pass it on to the jobs/runs it creates
- Works for caos jobs started from a claude cloud session

## Keys

Each writer has an ed25519 ref writer key. The private key stays on the
writer's device, in the checkout's git config as `caos.ref-writer-key`.

```sh
caos-cli ref-writer-key new    # private key; the public one goes to stderr
caos-cli ref-writer-key show   # public key
```

## Namespaces

A namespace is a group of protected refs that share one list of writers:

```text
refs/caos/protected/<ns>/writers      who can write the namespace
refs/caos/protected/<ns>/<anything>   the refs it protects
```

Protected refs can point at anything. `writers` is special: it always points at
a commit whose tree has a `.caos/writers` file listing who can write the
namespace, so its history is the history of write access. `.caos/writers`
looks like this:

```text
# <ed25519 pubkey, hex>  <label, display only>
3b6a27bcceb6a42d62a3a8d02a6f0d73653215771de243a63ac048a18b59da29  malcolm
9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60  nishad
```

`<ns>` is the hash of the first commit on `writers`, so you don't pick it. To
make a namespace, make a root commit listing yourself and push it to
`refs/caos/protected/<its hash>/writers`. Since the id hashes the initial list,
nobody can grab a namespace before you or make one you're not in. The first
commit is deterministic (author and committer `caos <caos>` at time 0, message
`caos namespace\n\n<label>`), so the same writers and label always give the
same id. The label is how one writer gets many namespaces: a different label
gives a different id. It has nothing to do with the display labels in the list.
`caos-cli namespace new [<label>]` does all this.

To change the list, push a new commit on `writers`
(`caos-cli writers add|remove <namespace|conversation> <key> [<label>]`). The
server only accepts it if someone already on the list signed it and it's a
fast-forward, so the history of who added whom sticks around. Removals apply
from the next push.

The server also only lets a namespace's writers move its other refs.

## Writer signatures

A push is a list of ref updates: for each ref, the hash the pusher expects it
at now (`<old>`) and the hash to move it to (`<new>`). Every push to a
protected ref signs those updates, in a push option (`git push -o`):

```text
caos-auth=sig:<pubkey>:<expiry>:<signature>
```

The signature covers the expiry and each `<old> <new> <ref>`, sorted by ref, so
it signs the ref moves themselves, not the commits. Pushes already use
`--force-with-lease`, so a captured signature only replays while the ref is
back at `<old>`; the ten-minute expiry covers that.

`caos-cli ref-push <rev> <ref>` pushes one ref, signed.

## Run tokens

Jobs never hold keys. A job writes for the writer whose request started it,
with a run token in place of a signature:

```text
caos-auth=run:<token>
```

When the server dispatches a job with write access, it mints a token, records
`token -> (writer pubkey, namespaces)` and drops it at `/secret/caos-write`; it
dies with the job's container. The server checks the token's pubkey against
`writers`, so removing someone also cuts off their running jobs. A worker's
`GitStore` sends the token on its own.

A job asks for write access with a `writes` arg: namespace ids, or `*` for
everything its creator got. No `writes`, no token, so e.g. a shell tool never
gets one.

A top-level request names its writer in a header, signed over the expiry and
the request's method and target:

```text
X-Caos-Write: <pubkey> <expiry> <signature>
```

The token covers `writes ∩ available`, where `available` is everything that
writer can write for a top-level request, and what the creator was granted for
a continuation or child. So a job can only pass on what it has: an agent step
holding a namespace can put it in `writes` for the jobs it starts, but a shell
it starts gets nothing, and neither does anything that shell starts. (The grant
rides along with `secrets::Context`, which already reaches sub-runs.)

A token can't change `writers`: jobs write content, writers decide who writes.
It can found a new namespace that lists only its own writer.

A job that only passes writes on asks for `*`. The suite does: its client signs
as a fresh writer, and `dev/run-tests`, `dev/run-test` and each test hold
`writes=*`, so a test can hand its steps the namespaces they write.

A cache hit runs nothing, so it writes nothing. An uncached job without access
runs without a token, and its push is refused.

## The hook

The server enforces all of this in a pre-receive hook,
`<git dir>/hooks/pre-receive`, that re-runs the server binary. It's a repo
hook, so it covers smart HTTP and iroh alike. The server's own ref writes
(`refs/caos/res/*` etc.) skip `receive-pack`, so the hook never sees them.

| Ref | Accepted when |
| --- | --- |
| content-named: `refs/caos/req/<h>`, `refs/heads/caos-test/<h>` | it points at `<h>`, or is deleted |
| unprotected (`caos.unprotectedRef`; `refs/caos/dev` by default) | always |
| `refs/caos/protected/<ns>/writers`, create | `new == <ns>`, `new` has no parents, pusher is in `new`'s list |
| `refs/caos/protected/<ns>/writers`, update | signer is in `old`'s list, fast-forward |
| `refs/caos/protected/<ns>/<other>` | signer's (or token's) key is in the current `writers`; a token must cover `<ns>` |
| anything else | rejected |

One blob read per namespace touched. Needs `receive.advertisePushOptions=true`,
which the server sets.

To roll this out on a server with writers the table misses,
`git config caos.refWriters report` makes the hook log what it would refuse and
let it through.

## Claude cloud

- Add `--ref-writer-key=<key>` to the setup line, next to `--secret-readers`;
  bootstrap writes it to git config. It's a setup arg, not an env var, for the
  same reason `--server` is: setup can't see env vars
  (integrations/claude-code/cloud/README.md).
- `caos mcp` signs its pushes with it and sends `X-Caos-Write` on its requests,
  which is where the jobs they start get their access.
- Nothing is stored per conversation, so a fresh container is fine.

## Conversations

A conversation lives in a namespace:

```text
refs/caos/protected/<ns>/writers
refs/caos/protected/<ns>/conversations/<hex id>/head
refs/caos/protected/<ns>/conversations/<hex child id>/head    a subagent
```

- One list covers the conversation and all its subagents, so removing someone
  removes them from all of it at once.
- `<ns>` comes from the creator's key and the conversation id (writers: the
  creator; label: `conversation <hex id>`), so a client finds its own without a
  lookup. Someone else's turns up by listing
  `refs/caos/protected/*/conversations/<hex id>/head`.
- Ids don't change (`cc/<session>`, `talk-1`, …). Across a process boundary a
  conversation goes by its address, `<ns>/<id>`: `llm-step`'s
  `--conversation`, the `X-Caos-Conversation` header, and a secret's
  `reader:@=<path> conversation=<address>`. A bare id could match a
  conversation of that name in anyone's namespace.
- A subagent's head goes in its parent's namespace. `llm-step` keeps its own
  `writes` on the child's request and puts the namespace on the relay's, so no
  keys get minted at runtime.
- The sidebar is
  `refs/caos/protected/<personal>/memberships/{active,archived}/<ns>/<hex id>`,
  in a personal namespace from the key (label `personal`).
- `caos-cli conversation-ref <id|address>` prints a conversation's head ref.

Multiplayer:

1. Nishad sends Malcolm their public key (it's not secret)
2. Malcolm adds it: `caos-cli writers add <conversation> <key>`, or
   `/invite <key>` in the tui
3. Nishad opens the conversation by its address from their own tui or session

Everyone's pushes and jobs use their own key, so the log shows who drove each
turn.

## Actors

An actor (std/actor/README.md) keeps its state on a branch, and that branch
lives in a namespace too:

```text
refs/caos/protected/<ns>/actors/<name>
```

- Many actors can share a namespace, so one `writers` change covers all of
  them. An actor a conversation drives can live in that conversation's
  namespace, where `llm-step` already holds `writes`.
- The caller puts `writes=<ns>` on the actor request. `finish`, the half of the
  wrapper that moves the branch, is granted a run token from it. The wrapper
  adds no `writes` to the inner's request, so a pure inner gets no token.
- The wrapper moves the branch by POSTing one command to `git-receive-pack`
  rather than running `git push`, so it sends the token itself: it asks for
  the `push-options` capability and sends `caos-auth=run:<token>` after the
  command.
- The namespace has to exist before the first write (`caos-cli namespace new`,
  or a job holding `*` founds one listing its writer). The actor itself still
  needs no start message: its first request finds an empty branch.
- Reads don't change: anyone can read an actor's state.

## Migration

| Pushed before | Now |
| --- | --- |
| `refs/caos/req/<h>` | unchanged, content-named |
| `refs/heads/caos-test/<sha>` | unchanged, content-named |
| `refs/caos/v3/conversations/…`, `refs/caos/v3/users/…` | namespaces, as above |
| `refs/heads/actors/<name>` | `refs/caos/protected/<ns>/actors/<name>` |
| `refs/caos/dev` | unprotected by default |
| tests pushing `refs/heads/*` | `caos-cli namespace new` + `ref-push`, or a content-named ref |

Conversations under `refs/caos/v3/` stay in the repo, unlisted. Nothing moves
them.

## Alternatives considered

- One keypair per ref, shared as an invite. Removing someone means moving
  everyone to a new ref, you can't tell writers apart, and a cloud session
  can't hold a key per conversation.
- Writers' private keys in the secret store, for jobs to sign with. Hands the
  server everyone's keys, and grants by image, so there's no way to say "pass
  this on to my children".
- The conversation id as the namespace id. Subagents would need namespaces of
  their own, and `cc/<session>` would need a lookup in every hook process.
- The writers list in the commit message instead of a file. It would save the
  hook a tree read, but those are local and cheap, and a file diffs with
  ordinary git tools and leaves room for more `.caos/` files beside it.
- The rules on the namespace's own ref, `refs/caos/protected/<ns>`, with its
  refs under it. Git can't do both: a ref can't also be a directory of refs
  (`refs/a` and `refs/a/b`), so the list gets a fixed name inside instead.
- Git's own signed pushes (`git push --signed`). A push certificate signs the
  same `<old> <new> <ref>` list plus a nonce from the server, which beats an
  expiry. But every client needs a GPG keyring or `ssh-keygen` (a Claude cloud
  container has no `ssh-keygen`), the nonce costs a round trip first (the
  actor's one-POST push can't make one), and jobs can't sign at all, so run
  tokens would still need a push option.

## Open questions

- `refs/caos/dev` is unprotected: any job can rewrite what dev-mode bootstrap
  installs. An operator namespace whose id bootstrap is given would fix that.
- A signature can be replayed on another server with the same namespace until
  it expires. Putting the server's identity in the signed text would fix that,
  if it matters.
- `writes=*` is broad on purpose: a job holding it can write every namespace
  its writer can.
