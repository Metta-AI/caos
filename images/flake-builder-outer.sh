#!/usr/bin/env bash
# The OUTER flake-builder (design/flake-images.md, catch 2): the two-stage
# worker (it carries /caos) that turns a flake tree into a runnable worker
# image. One file, branching on a curried-in --stage:
#
#   stage 1 (default): run the INNER (std/flake-builder-inner) on the flake
#     tree to get the CLEAN image {ref, config} (nix build, streamed to the
#     registry, keyed on the flake tree alone), then continue at stage 2.
#   stage 2: stack the caos runner layer on the clean image, emitting a
#     git-docker delta tree {base: docker://<clean>, config.json, layer00:
#     /bin/caos setuid + /tmp + userdb}. The server converts that delta (cheap:
#     one caos layer on a docker base) into a registry digest.
#
# Curried onto std/bash (which carries /caos), published as std/flake-builder.
# Split this way so the expensive inner build is keyed WITHOUT caos (stable
# across caos edits); only this cheap stack+convert re-runs when caos changes.
set -euo pipefail

fail() { echo "FLAKE OUTER FAIL: $*" >&2; exit 1; }

# Args are lazy placeholders — fetch before reading. Stage 1 (the initial
# invocation) carries no --stage, so the fetch fails and we default to 1.
stage=1
if caos get /cas/args/stage 2>/dev/null; then
  stage=$(cat /cas/args/stage)
fi

if [ "$stage" = 1 ]; then
  # Stage 1: run inner(flaketree) -> {ref, config}; then run stage 2 with the
  # same flake tree as --in and the inner's result as --result (run-then's
  # then(--in, --result) shape). The stage-2 continuation is this same script
  # re-curried onto std/bash with --stage=2.
  then2=$(caos curry /cas/std/bash -- "--script:@=/cas/args/script" "--stage=2") \
    || fail "currying stage 2"
  caos run-then /cas/args/in -- \
    --run=/cas/std/flake-builder-inner \
    --then="$then2"
  exit 0
fi

# Stage 2: --result = the inner's {ref, config}; --in = the flake tree (unused).
caos get -r /cas/args/result
clean=$(cat /cas/args/result/ref) || fail "no clean ref in the inner result"
[ -s /cas/args/result/config ] || fail "no config in the inner result"

img=/tmp/img
l=$img/layer00
mkdir -p "$l/usr/bin" "$l/bin" "$l/tmp" "$l/etc"

# The setuid caos runner gateway. Git trees can't encode setuid, so a
# `<name>.caosmeta` sidecar (JSON {mode,uid,gid}) beside the entry carries it;
# the server applies it when it rebuilds the layer tar (apply_layer_metadata in
# the convert). The sidecar sits in the entry's PARENT dir.
cp /bin/caos "$l/usr/bin/caos"
printf '{"mode":"4755","uid":0,"gid":0}' > "$l/usr/bin/caos.caosmeta"
# A scratch/bare flake base has no /bin merge, and runnerd forces
# --entrypoint /bin/caos — so create the link explicitly (a real git symlink,
# preserved as-is by the convert).
ln -s /usr/bin/caos "$l/bin/caos"
# The world-writable /tmp the unprivileged worker scratches in. An empty dir
# stores as an empty tree; its 1777 mode rides in the sibling sidecar.
printf '{"mode":"1777","uid":0,"gid":0}' > "$l/tmp.caosmeta"
# The user db (the runner drops to uid 1000 unless the image grants root).
printf 'root:x:0:0:root:/root:/sbin/nologin\nworker:x:1000:1000:caos worker:/tmp:/sbin/nologin\n' > "$l/etc/passwd"
printf 'root:x:0:\nworker:x:1000:\n' > "$l/etc/group"

# The delta stacks on the clean image; config.json is the inner's massaged
# config (flake Env preserved, runner entrypoint, /bin on PATH).
printf 'docker://%s' "$clean" > "$img/base"
cp /cas/args/result/config "$img/config.json"

caos put "$img" /cas/out
