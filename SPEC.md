# Work -- request and response

A WorkRequest is:
- an ArgTree: a git tree containing named args:
    - image: See below
    - other args, by agreement between the caller and the worker, all as git files/trees/commits
    - std (optional, but very common)
    - salt (optional): a string that is used to invalidate the cache
- stack
- trace id

A WorkResult is a git object, containing whatever the worker chose to return

WorkRequest contains ArgTree, stack, trace id, etc #todo
- The ArgTree is the cache key

We generally talk about ArgTrees, not images. An image is just one arg (see
below), so one simple ArgTree is one that only contains an image; richer
ArgTrees carry other args alongside it. Passing around ArgTrees rather than
images is what makes currying (below) a uniform operation.

Also note in calling that rebinding existing args is an error #todo

# Forming an ArgTree

The simplest ArgTree is one that only specifies an image

## Docker digest

image = "docker://<docker url>" (a string), using a hash/digest, not a tag

## A flake

image = git tree, containing a flake.nix and flake.lock, which are used to build the image

## Git-tree image

image = git tree with the following structure:
- base: an image ref
- overlay (optional): a git tree that will sit on top of the base
    - non-standard ownership or perms can be represented with a sidecar foo.caosmeta file for a given file foo
- env: #todo

## Currying

Currying takes an ArgTree and args and returns a new ArgTree, binding the existing args and the new args

Curry shall fail if passed an arg that is already defined in the WorkRequest

# Workers

When caos gets a WorkRequest, it builds the image specified by the ArgTree. When doing this, it adds a few pieces to the image:
- /bin/caos: the binary used by the worker to communicate with caos
- a worker user, distinct from root, so that caos can prevent the worker from tampering with content-addressable files

Add more about the contract with the worker #todo
- get/put, unreadable files, +x preservation
- /cas/out
- run-then, map-then

# Principles of reliability

Caos is reliable because:
- It dies when unexpected things happen, rather than trying to recover from errors that we didn't anticipate or don't understand
- It checks for and fixes expected issues
    - If we expect a directory, create it if necessary each time we start
    - If we expect settings on a git repo, set them each time we start
- Work is deterministic
    - The results are cached, so they need to be at least deterministic enough to satisfy the caller. For example, tests include timing info and llm results are random, but both are sufficiently deterministic for their callers

# Principles of performance

Caos is fast because:
- It caches work based on the ArgTree. The same work is never run a second time
- It takes pains to narrow trees before using them as keys, to avoid cache misses
    - For example, caos-tools/build.sh narrows the tree to just what the flake needs to build the stackbuilder image. Then it passes just the source files when running the stack-builder to build a stack
    - Compare with calculating custom keys based on a subset of the data: this causes stale values when it goes wrong
    - Compare with
- It calculates keys quickly:
    - Calculating nix paths on a cold worker took 10+ seconds. We no longer do that
    - We make git's hash code fast -- even in debug builds (the default at the moment), we are careful to keep our dependencies optimized so that hashing is fast
- It sips from git. Instead of materializing a whole tree or subtree, each step only loads what it needs
- It pushes to git only what's new

Two things need to be fast:
- Primary: Rebuild and retest everything: `time CAOS_SALT=$(date --iso=s) result/bin/caos-cli run-tool test`
    - This doesn't rebuild the stack-builder image from the flake, because that's just a function of the flake and is cached in docker
- Secondary: Build and restart on the host: `time nix build && time result/bin/caosd up`

We have various kinds of salt to control what work gets redone:
- `CAOS_SALT=$(date --iso=s)` to rerun all caos workers (but not rebuild flakes, which do not include this in their key)
- `run-tool test --test-salt=$(date --iso=s)` to rerun all tests

If these become slow:
- Sample `ps` during a run

# Codebase

## Before committing

Run `time result/bin/caos-cli run-tool test`

## General
- A root-level flake.nix and flake.lock SHALL install all of the dependencies needed to build the code
- For now, the code is written in Rust. This was probably a mistake:
    - Rust can fetch dependencies just based on a cargo file, but it won't compile them without top-level sources. This is the difference between roughly 3 seconds and 12 seconds to compile cold. Go doesn't have this problem
    - Rust compiles somewhat slowly compared to Go
    - Rust's toolchain is much larger than Go's, and includes less, so we need to fetch even more
- For now, we write scripts in Bash. This was probably a mistake:
    - Humans (Malcolm) definitely have trouble reading and writing bash
    - But the bots do too! They've started keeping notes on sharp edges with Bash in AGENTS.md
    - Go supports `go run`, which compiles and runs a .go file in one shot
- In the future, The code will be written in go, including scripts, which SHALL be run by `go run`
- Comments should focus on decisions that a naive reader might undo instead of narrating the journey that a bot took to arrive at a solution

## Worker scripts

- Workers shall be written in a single file that covers all the stages of the worker. That is, when a worker calls map-then or run-then, the next next stage should be in the same file. The worker shall use a `stage` argument to track which stage it is up to

# Server

- Has a local git repo
    - GC is disabled
- shall listen on port 80 and respond to requests:
    - Git push/pull requests are routed to git to handle against the repo
    - WorkRequests as described below. The input is the hash of the WorkRequest as a git tree. The result is the hash of the WorkResult

# Misc

- `run-tool` does not fetch the output of the tool that it runs. It just prints the hash and the stdout part
- `caos put` checks whether the server has each object while descending the tree, to avoid putting things that are already there
- Mentions of `refs/caos/bins` are inconsistent. Does it still exist?

# From agents

Agents, add more notes here
