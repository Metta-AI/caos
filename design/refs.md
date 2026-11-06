# Named refs for workers: `ref-get`, `ref-set`, `ref-then`

Everything a worker handles is addressed by content, with one deliberate
exception: a **named, mutable ref**. Some state has to outlive a request
(`std/actor` keeps an actor's state on a branch), and some jobs have to signal
each other. Both used to reach for git: `git ls-remote` to read a ref, and a
hand-built receive-pack command to move one. These verbs give every worker the
same thing as one command each.

## The verbs

```text
caos ref-get <ref> <cas-path>
caos ref-set <ref> (--new=<hash> | --new:@=<cas-path> | --delete)
                   (--expected=<hash|absent> | --expected:@=<cas-path> | --force)
```

- `ref-get` records the commit the ref names at `<cas-path>`, kind-tagged
  `commit` exactly as `put-commit` records one, and exits 0. From there it is
  an ordinary CAS value: `caos hash` gives its id, and `--name:@=` passes it
  on. **A missing ref exits 2 and writes nothing**; a real failure exits 1, so
  absent and broken stay apart (AGENTS.md). Go workers use `w.RefGet`, which
  answers false for 2. It never waits.
- `ref-set` moves the ref to a commit, or deletes it with `--delete`. **A
  commit is given either way**: as a hash with `=`, or as the CAS path it is
  recorded at with `:@=`, the spellings `run` and `curry` use. So the commit
  `put-commit` just minted and the head `ref-get` just read pass straight
  through, and a script never has to handle a hash:

  ```sh
  caos ref-get R /cas/old                      # exit 2: no such ref
  caos put-commit /tmp/commit /cas/new         # its parent is $(caos hash /cas/old)
  caos ref-set R --new:@=/cas/new --expected:@=/cas/old
  ```

- `ref-set` is a **compare-and-swap by default**: `--expected` names the value
  being replaced, `absent` meaning create-only, and the server checks it under
  the ref's lock. A stale expectation fails with the value the ref actually
  holds. An unconditional write must say `--force`; leaving both out is an
  error, so an overwrite is never an accident.
- `absent` appears only in `--expected`, the same word `caos push-git
  --expected=absent` uses. Deleting is its own flag rather than a magic value.

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

## Waiting: `ref-then`

```text
caos ref-then <ref> [--expected=<hash|absent> | --expected:@=<cas-path>]
              [--timeout=<secs>] [--then:<type>=<image>] [--catch]
```

A worker that needs to wait for another job to move a ref must not poll for
it. A container looping on `ref-get` holds its runner slot while the writer
it is waiting for may be queued for one: the hold-and-wait that
`design/map-then.md`'s deadlock argument rules out. So waiting is a
**continuation**, like `run-request-then`:

- The worker records `ref-then` as its final act and exits. Its slot is free.
- The server's promise interpreter blocks a request thread until the ref holds
  something other than `--expected` (default `absent`, so by default it waits
  for the ref to be set), then runs `--then` with `--result`: the new commit,
  as a commit entry exactly like what `ref-get` records. The callback reads
  `/cas/args/result` like any commit-valued arg, and can wait for the change
  after that one with `--expected:@=/cas/args/result`. With no `--then`, the
  commit is the request's result.
- A ref **deleted** while it was waited on, which can only happen when
  `--expected` names a commit, reaches `--then` as a bare `--absent` flag and
  no `--result`. With no `--then`, the request's result is then an empty blob.
- `--timeout` bounds the wait: 600s by default, a day at most. A timeout fails
  the wait, and `--catch` hands that failure to `--then` as `--error` instead,
  so the callback can report what it was waiting for.
- **A request that waited on a ref is never cached.** Its answer depends on
  when it ran, not on its ArgTree. It uses the same flag a caught failure
  does.

A write through `ref-set` wakes waiters at once. One that went round it (a git
push to the server) is noticed by a once-a-second recheck.

An in-flight wait lives on a server thread, as every continuation's
resolution does, so a server restart drops it along with the rest of the
request. `ref-then` adds no durability problem that `run-request-then` does
not already have.

`tests/sub-run` uses it to prove that detached work outlives its caller with
a handshake rather than a fixed sleep: the test sets a ref only after the
caller has exited, the detached job waits for it with `ref-then` and answers
on a second ref, and the test waits for that with `ref-then` in turn.

## Server

`GET /ref?name=<ref>` answers `<oid>` or `absent`. `POST /ref` takes `{"name",
"value", "expected"}`, where a missing `expected` is an unconditional write,
and answers 409 on a stale one. Both live in `rust/crates/server/src/refs.rs`,
over `git for-each-ref` and `git update-ref`.
