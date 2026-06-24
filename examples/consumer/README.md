# Using caos from another tree

This is a minimal flake for a project that *consumes* caos rather than being
caos. It adds caos as a flake input and puts the caos commands on the dev
shell's PATH: `caos-cli` (drive workers) and `caosd` (start the stack, which
also publishes the builtin worker library on startup). Enter the shell once
with `nix develop`, then run them as plain commands — no `nix run`/`nix build`.

## One-time

Point the `caos` input at the real repo (e.g. `github:redheron/caos`) instead
of the relative path used here.

## The loop

```sh
# 0. Enter the dev shell: caos-cli/caosd/set-stdlib on PATH, CAOS_SERVER_URL set.
nix develop

# 1. Bring the stack up (redis + registry + caos server). Foreground; Ctrl-C
#    stops it. Server state (the bare git repo) lives in ./.caos-data — override
#    with CAOS_DATA. This also publishes the builtin stdlib on startup.
caosd

# 2. In another shell (also `nix develop`), point caos-cli at the server. It
#    runs inside a git working tree that has the server as its `caos` remote:
git init                              # if this tree isn't a repo yet
git remote add caos "$CAOS_SERVER_URL"

# 3. Run a builtin worker, or build your own from a Rust source via the rustc
#    worker (see the caos repo's test-rust-worker.sh for the curry/run pattern):
caos-cli run docker://caos-worker-hello:latest "$CAOS_CAS_DIR/out" -- --data:@=somefile
```

## Updating the stdlib

`caosd` republishes the builtin library on every startup (rebuilding the images
— a cache hit when unchanged — and atomically repointing `refs/caos/std`), so
after editing a builtin worker in the caos tree just restart `caosd`.
