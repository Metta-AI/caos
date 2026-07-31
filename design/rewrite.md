# Preamble

Caos is a Content-Addressable Operating System. It's functional programming with git as the values and docker as the
functions, cached by redis

## Why?

### Security

Every package in your supply chain, and your agent, runs with full access to your computer (by default) and the auth tokens to that allow you to interact with github and many other services

Caos runs all of these pieces in separate containers, with just the permissions that they need

### Performance

Today, when you or an agent make a change, you clone the repo or make a new worktree, then edit a file. You build and test everything. Unless you use bazel, it doesn't matter that the CI server has built and tested most of this already. You do it again, unless (maybe) you have a cache from a different worktree. When you push your code, the CI server checks it all out and builds and tests it all again. Or it copies a large cache file, that often isn't quite the right version, and still builds and tests more than you needed

Caos breaks building and testing into small pieces and caches the results. When you build and test, we never materialize the whole tree

### Location independence

Today, most people run most of their agent workloads on their local machine for convenience. When the work no longer fits, they buy a desktop and try to interact with it over tmux. If the work grows further, they have to split it up between cloud instances. If an agent wants to spin up subagents on other computers, it gets even more annoying

Caos runs work well-defined binaries with well-defined inputs and well-defined environments. The work can move seamlessly between computers

## What

* Your code is already in git. You already know docker
* Caos provides to glue to use git as a distributed file system and docker containers as functions. We cache the results in redis
* Workers (containers) receive their inputs as git objects, and lazily load only as much as they need. They stage their results into git. None of this is committed or clogs your main git repo
* Workers can call other workers. They can also define other workers in their git return values (similar to functional programming)
* Once we've run a worker with an input, we cache the mapping to the output value and reuse it for future requests

# Spec

## Work -- request and response

A WorkRequest is:
- an ArgSet: a git tree containing named args:
  - image: See below
  - other args, by agreement between the caller and the worker, all as git files/trees/commits
  - std (optional, but very common)
  - salt (optional): a string that is used to invalidate the cache
- stack
- trace id

A WorkResult is a git object, containing whatever the worker chose to return

WorkRequest should contain ArgsSet, stack, trace id, etc #todo
- The ArgsSet is the cache key

Also note in calling that rebinding existing args is an error #todo

## Image

Work is run in a container, described by an image. There are several forms of this:

### Docker digest

"docker://<docker url>" (a string), using a hash/digest, not a tag

### Git-tree image

A git tree with the following structure:
- base: an image ref
- overlay (optional): a git tree that will sit on top of the base
  - non-standard ownership or perms can be represented with a sidecar foo.caosmeta file for a given file foo
- env: #todo

### A flake

Also a git tree, but containing a flake.nix and flake.lock, which are used to build the image

### Currying

Importantly, we don't pass around images. Instead, we pass around ArgSets. These might contain only an image, or they might contain other args. Thus, we can easily support a curry operation that takes an ArgSet, and returns a new ArgSet, now with more args

Curry shall fail if passed an arg that is already defined in the WorkRequest

### Caos overlay

When caos is asked to run any docker image, it adds in its own overlay, containing /bin/caos, /etc/passwd and /etc/group. #todo more here

## Principles of reliability

Caos is reliable because:
- It dies when unexpected things happen, rather than trying to recover from errors that we didn't anticipate or don't understand
- It checks for and fixes expected issues
  - If we expect a directory, create it if necessary each time we start
  - If we expect settings on a git repo, set them each time we start
- Work is deterministic
  - The results are cached, so they need to be at least deterministic enough to satisfy the caller. For example, tests include timing info and llm results are random, but both are sufficiently deterministic for their callers

## Principles of performance

Caos is fast because:
- It caches work based on the ArgSet. The same work is never run a second time
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

## Codebase

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

## Server

- Has a local git repo
  - GC is disabled 
- shall listen on port 80 and respond to requests:
  - Git push/pull requests are routed to git to handle against the repo
  - WorkRequests as described below. The input is the hash of the WorkRequest as a git tree. The result is the hash of the WorkResult

## Misc

- `run-tool` does not fetch the output of the tool that it runs. It just prints the hash and the stdout part
- `caos put` checks whether the server has each object while descending the tree, to avoid putting things that are already there

# From agents

Agents, add more notes here

## The object graph is not the dependency graph

Fetching a value's git closure brings its subtrees. It does NOT bring anything
the value merely *names*, and caos names things three ways that the closure
misses: a curry's `base` is a blob holding a hash; a bound literal like
`--cargo=<hash>` is a blob holding a hash; and a flake tree makes the server
resolve `flake-builder` **by name**. So "hand this job std/rgrep" does not hand
it the runner that rgrep is curried over.

This deserves to be a spec rule, because the natural assumption — a value is
self-contained once you have its closure — is false, and it fails at a distance:
the job starts, then dies on a missing object or an unresolvable name.

## Cargo: `-p <member>` is not a subset of `--workspace`

Feature unification differs between them, so the same dependency gets different
feature flags, a different fingerprint, and a recompile. Any per-crate
decomposition therefore re-does the dependency graph unless the bake pre-warms
each member's resolution — measured at 114 dependency checks against 12. Three
commands, because check/clippy/test each have their own artifact kind.

Another entry for "Rust was probably a mistake": the unit of caching the
language gives you is not the unit you want to distribute.

