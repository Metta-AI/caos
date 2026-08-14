Read ./SPEC.md

# Shell, under `set -euo pipefail`

Every script here runs with it, and two constructs quietly break under it.

- **`[ cond ] && action` exits the script when the condition is false**, if that
  list is the last command in its scope (a function, a loop body, a `{ }` block,
  the script). Write `if`. This is not hypothetical: it is the single largest
  source of bugs in this tree's shell — `build-builtins.sh` still carries three
  latent instances, one of which makes `./build-builtins.sh <name>` exit 1
  before doing anything.
- **`pipefail` makes a pipeline fail on its LEFTMOST failure**, not its last
  command. `curl -f ... | awk` returns curl's 22 on a 404, so a lookup whose
  "absent" answer is a 404 dies instead of returning empty. Distinguish absent
  from broken explicitly — a swallowed error here silently hides an unreachable
  service.
- **A comment inside a backslash-continued command severs it.** An env prefix
  written one-assignment-per-line (`FOO=1 \` … `bash serve &`) breaks the
  moment a comment lands between two of those lines: the continuation joins
  INTO the comment and the command runs with none of the environment. It is
  not a parse error, so `bash -n` passes and the damage shows up far away —
  in `test-stack/worker` it was a stack dying at bring-up with "serve needs
  CAOS_STACK_STATE", 28 clients polling for an address that never appeared,
  and a suite that read as SLOW rather than broken. Put the comment above the
  block.
- **`${VAR:?message}` is word-split unless you quote it.** An apostrophe in
  the message (`${X:?the socket's path}`) opens a single-quoted string that
  never closes and swallows the REST OF THE FILE; bash then reports an
  unterminated `if` hundreds of lines away, nowhere near the cause. Write
  `X="${VAR:?message}"` — inside double quotes the message is literal.
- **A worker script only has what its image's flake lists.** `std/bash` is
  bash, coreutils, diffutils, gnugrep, findutils and jq — there is **no
  `sed`**, and no awk. `sed 's/^/  /'` in `caos-tools/test.sh` passed two
  green suites before it fired, because it only ran on the failing-test path:
  the report that exists to explain a failure was the thing that broke. Prefer
  a bash loop (`while IFS= read -r line`), and when reaching for a binary,
  check `std/<image>/flake.nix` first — an absent one is exit 127 at runtime,
  not a build error.

# Nix

- **Everything an image's `contents` puts on disk is a SYMLINK into
  `/nix/store`.** `dockerTools` lays contents down with `lndir`, which mirrors
  directories and symlinks files. So `cp -R` out of an image layout copies
  links, not content, and the copy is read-only and store-shaped. Use `cp -RL`
  — `build-builtins.sh` learned this for published flake trees, and `serve`
  learned it again for the seeded git dir, where a symlinked `HEAD` made git
  call a perfectly good repo "not a git repository" (`validate_headref` accepts
  a symlinked HEAD only if the target starts with `refs/`) while gix opened it
  and the server started anyway.
- **`runCommand name {} '' … '' + s` parses as `(runCommand name {} '' … '') + s`.**
  That concatenates the DERIVATION with the string, so you get a store path
  with your script text glued to the end — and it evaluates, builds, and fails
  much later with something like `<store-path># my comment: No such file or
  directory`. Parenthesize the whole script when appending to it.
- **`nix build` with TWO installables and TWO `-o` flags does not create both
  symlinks.** `nix build .#a -o result-a .#b -o result-b` leaves one pointing at
  a previous build, and the other can be missing entirely — so you read a stale
  binary and blame the code. Build one output per invocation. (`--refresh` is not
  needed: nix picks up dirty-tree edits fine.)

# Git

- **A `git fetch` can fail over an object it never asked for.** The post-fetch
  connectivity check is `rev-list --not --all --alternate-refs`, so it walks the
  tips of every ALTERNATE object store too. The test harness gives each client
  repo an alternate holding a deliberate SUBSET (`tests/lib/run-test.sh` →
  `/tmp/seed-git/objects`, "exactly what this test declared"), so any fetch there
  dies with `missing blob object <x>` naming an object with nothing to do with
  the fetch — and blames the fetch. It cost a session on `tests/remote-ref`: the
  identical `git fetch` succeeds in any ordinary repo and fails in the harness's
  client repo, so the difference is the alternate, not the command. Add `-c
  core.alternateRefsCommand=true` when the fetched closure stands alone
  (`:@@=`'s `--depth 1` fetch does); do NOT add it where the alternate's tips are
  legitimately part of the history you are completing.

- **An unsalted `run-tool test` does not prove a push works.** `ensure_pushed`
  pushes `<argtree>:refs/caos/req/<argtree>`, so re-running with an unchanged
  tree pushes a hash the server already has at that ref: git sends nothing,
  traverses nothing, and any defect in packing the request is invisible. Only a
  NEW ArgTree builds a real pack — which is why the primary gate is
  `CAOS_SALT=$(date --iso=s) result/bin/caos-cli run-tool test` (SPEC.md) and
  why a green unsalted suite once sat next to a hard-failing salted one for a
  whole session. Run the salted form before believing a push-path change.
- **The test harness hides object-availability bugs.** Each per-test client repo
  gets an ALTERNATE object store (`tests/lib/run-test.sh` → `/tmp/seed-git`)
  holding what that test declared, so a client that could not otherwise read its
  base image's closure reads it anyway. A test that is ABOUT what the client
  holds must `rm .git/objects/info/alternates` first — see `tests/push-closure`,
  which needs a rustc-built worker (whose base is reached by unwrapping a curry,
  and so is covered by no advertised ref) to reproduce at all.

# Caches and defaults, when a suite fans out

Both of these presented as "the tests are slow", cost a long time to find, and
are the kind of thing that is invisible until 29 clients arrive at once.

- **A redis cache read followed by a redis cache write is CHECK-THEN-ACT.** The
  server answers on a thread pool, so a fan-out means every client misses the
  same key in the same instant and every client does the whole job. Measured:
  29 tests produced 29 identical `converted image` lines, each materializing a
  ~200 MB tree to a temp dir, tarring it, hashing it and pushing it — 24 seconds
  in which the suite started no containers. Single-flight anything expensive
  behind a per-key lock and RE-READ the cache after acquiring it. There is one
  server process per stack, so an in-process lock is the whole requirement.
- **A default is dead if every caller passes the variable.** `test-stack/worker`
  read `CAOS_TEST_STACK_IDLE_SECS` with a 900s default, and the `docker run`
  three hundred lines away that starts that role always passed the variable,
  defaulting it to 120. The 900 was decoration; every shared stack died two
  minutes after the suite that created it and every later run paid a cold start.
  When a value is read in one place and supplied in another, the SUPPLIER wins —
  grep for the variable, do not read the default and believe it.
- **A dead container address does not refuse connections, it swallows them.**
  Nothing listens on an IP whose container is gone, and the bridge drops the SYN
  rather than sending RST, so anything that dials it waits out a TCP retransmit
  — `git ls-remote` sat there for 2.1s, three retries made 11.3s, and the caller
  did that twice. Bound any probe of an address that might be stale
  (`timeout 1 bash -c "exec 3<>/dev/tcp/$host/$port"`), and do not re-probe an
  address already proven dead — wait for it to CHANGE.

- **An idle machine mid-run means a job is waiting, and a `docker://seeded…`
  job is the one that waits in silence.** A seeded sentinel defers generic
  runners for its whole pending window (900s in a stack), so when its key
  doesn't match the seed record NOTHING happens — no container, no log line —
  and the eventual `no runner for arg_tree …` blames capacity, the one thing
  that isn't wrong. The server now proves this instead: a parked poll whose
  `required["base"]` equals the job's `base` IS that sentinel's seeder, so a
  disagreement is permanent, and after `CAOS_SEEDED_GRACE_SECS` (45s) the 503
  names the differing args. **Read that message rather than re-deriving it** —
  it prints both oids, and the fix is always one of the two sides being stale.
  Anything you hand-roll that forms a seeded key (a `caos run-then` against a
  sentinel, in `caos-tools/*.sh`) must match `build-builtins.sh`'s record
  exactly, `strip_caos_expr` included; nothing else checks that agreement.

# Before committing

- **Never read a gate's exit status through a pipe.** `cmd 2>&1 | tail`
  reports `tail`'s status, so a failed command looks like a pass — that happened,
  and the next step ran against a stack that was not up. Use
  `cmd 2>&1 | tail; echo "EXIT=${PIPESTATUS[0]}"`, or don't pipe.
- **`run-tool test` does not cover `nix build`.** The suite compiles the tree
  with cargo over the real `crates/` directory; `nix build` compiles a copy
  filtered by the flake's `src`. Anything that filter drops is invisible to a
  green suite — four `include_str!("githist/*.sh")` calls merged under a
  passing run and broke `nix build` on arrival. `tests/std-lint` now runs
  `./lint-flake-src.sh` for the embedded-file case; for anything else the
  filter touches, run `nix build` yourself before committing.
- If this doesn't catch everything, we need to add it to the above step

