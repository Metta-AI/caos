# Named refs for workers: `ref-get`, `ref-set`, `ref-then`

Everything a worker handles is addressed by content, with one deliberate
exception: a **named, mutable ref**. Some state has to outlive a request
(`std/actor` keeps an actor's state on a branch), and some jobs have to signal
each other. Both used to reach for git: `git ls-remote` to read a ref, and a
hand-built receive-pack command to move one. These verbs give every worker the
same thing as one command each.

## The verbs

```text
caos ref-get <ref>
caos ref-set <ref> <hash|absent> (--expected=<hash|absent> | --force)
```

- `ref-get` prints the ref's value: the commit it names, or `absent`. It never
  waits.
- `ref-set` moves the ref to a commit, or deletes it with `absent`. It is a
  **compare-and-swap by default**: `--expected` names the value being
  replaced, `absent` meaning create-only, and the server checks it under the
  ref's lock. A stale expectation fails with the value the ref actually holds.
  An unconditional write must say `--force`; leaving both out is an error, so
  an overwrite is never an accident.
- `absent` is the same word `caos push-git --expected=absent` uses.

## Rules

- **Only `refs/heads/` is offered.** The rest of the namespace is the
  server's own: `refs/keys/` is the secret store and `refs/caos/res/` pins
  results. The server refuses any other name, and so does the CLI.
- **A value is a commit already in the store.** Git requires that of a ref
  under `refs/heads/`; `caos put-commit` mints one.
- **They are ordinary git refs** in the server's repository, so `git
  ls-remote` and a receive-pack push see and move the same values.
- **Names are shared state.** Suite runs can reuse a test stack, so a test that
  writes a ref puts a run-unique prefix in its name and deletes what it wrote
  (`tests/README.md`).
- **Reading a ref is not a pure function of the ArgTree.** A worker that reads
  one and is then answered from the cache sees the old value. A caller that
  needs a fresh read makes its request unique (std/actor's `request-id`).

## Server

`GET /ref?name=<ref>` answers `<oid>` or `absent`. `POST /ref` takes `{"name",
"value", "expected"}`, where a missing `expected` is an unconditional write,
and answers 409 on a stale one. Both live in `rust/crates/server/src/refs.rs`,
over `git for-each-ref` and `git update-ref`.
