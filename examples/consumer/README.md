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
# 0. Enter the dev shell: caos-cli and caosd on PATH.
nix develop

# 1. Bring the stack up (redis + registry + caos server). Foreground; Ctrl-C
#    stops it. Server state (the bare git repo) lives in ./.caos-data — override
#    with CAOS_DATA. This also publishes the builtin stdlib on startup.
caosd

# 2. In another shell (also `nix develop`), add the server as the `caos` remote.
#    That remote URL *is* the server — caos-cli reads it from there, so there's
#    no CAOS_SERVER_URL to set. caos-cli must run inside a git working tree:
git init                              # if this tree isn't a repo yet
git remote add caos http://localhost:9090

# 3. Build your own worker from Rust source with the published std/rustc,
#    then run it — hello.rs (next to this README) is the whole worker.
#    CAOS_CAS_DIR is where results land. (Builtins run the same way by
#    their /cas/std/<name> path, e.g. /cas/std/bash-tool.)
export CAOS_CAS_DIR=/tmp/cas
git add hello.rs && git commit -m "my worker"
builder=$(caos-cli curry /cas/std/rustc -- --runner:@=/cas/std/runner)
caos-cli run "$builder" img -- --src:@=hello.rs
git add img && git commit -m "built worker"
caos-cli run "$(git rev-parse HEAD:img)" "$CAOS_CAS_DIR/out" -- --greeting=hi
```

## Updating the stdlib

`caosd` republishes the builtin library on every startup (rebuilding the images
— a cache hit when unchanged — and atomically repointing `refs/caos/std`), so
after editing a builtin worker in the caos tree just restart `caosd`.
