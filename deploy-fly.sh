#!/usr/bin/env bash
# Stand up (or update) a complete, isolated caos stack on fly.io.
#
# One stack = three long-lived apps plus on-demand per-worker apps, all named
# with the stack as the leading component (`<stack>-caos-...`):
#   <stack>-caos-server            caosd (compute + storage), a /git Volume,
#                                  public ingress at https://<stack>-caos-server.fly.dev;
#                                  its registry repo doubles as the shared worker-image repo
#   <stack>-caos-redis             redis:7 — result/image cache (6PN-only)
#   <stack>-caos-worker-<hash16>   one app per worker version, created by caosd
#                                  (machines pull from the server's shared repo, :w-<hash>)
#
# Multiple stacks coexist in one fly org because every app name leads with the
# stack: caosd passes its own worker prefix to the provisioner via
# CAOS_FLY_WORKER_PREFIX, so two stacks never fight over a worker app name. The
# matching teardown is `./teardown-fly.sh <stack>`.
#
# Idempotent: re-running skips anything already present and just rebuilds caosd
# and updates its machine to the fresh image. Safe to run repeatedly.
#
# Usage: ./deploy-fly.sh <stack> [region]
#   <stack>   lowercase [a-z0-9-], e.g. `mh2`, `prod`, `alice`
#   region    fly region (default: $CAOS_FLY_REGION, else sjc)
#
# Requires: .caos-dev/fly.env with CAOS_FLY_TOKEN (an *org* deploy token) and
# CAOS_FLY_ORG; flyctl and nix on PATH (skopeo is pulled via nix).
#
# Tunables (env or .caos-dev/fly.env): CAOS_FLY_POOL (worker pool size),
# CAOS_FLY_VOL_GB (/git size, default 10), CAOS_FLY_RAM_MB (caosd RAM, default
# 2048 — big enough for the rustc builtin's git push), CAOS_FLY_STD (which
# builtins to host: `all` (default), `none`, or a list like `hello bash`).
#
# Hosting the std library (build-builtins.sh -> sets refs/caos/std + loads the
# builtin image trees into caosd's CAS) lets clients resolve `/cas/std/<name>`.
# Default `all` hosts the full library on every deploy — it includes the heavy
# rustc image, so it relies on the 10GB/2GB volume/RAM defaults. (Independent of
# this, caosd always converts + registry-pushes + provisions a worker lazily on
# first run; running your own images needs nothing pre-hosted.)
set -euo pipefail
cd "$(dirname "$0")"
PROJECT=$PWD

STACK=${1:-}
[ -n "$STACK" ] || { echo "usage: $0 <stack> [region]" >&2; exit 2; }
[[ "$STACK" =~ ^[a-z0-9][a-z0-9-]*$ ]] || {
  echo "stack must be lowercase [a-z0-9-] (got: $STACK)" >&2; exit 2; }

# Credentials + defaults from the gitignored env file.
set -a; . "$PROJECT/.caos-dev/fly.env"; set +a
: "${CAOS_FLY_TOKEN:?set CAOS_FLY_TOKEN in .caos-dev/fly.env (an org deploy token)}"
ORG=${CAOS_FLY_ORG:-personal}
REGION=${2:-${CAOS_FLY_REGION:-sjc}}
POOL=${CAOS_FLY_POOL:-1}
VOL_GB=${CAOS_FLY_VOL_GB:-10}
RAM_MB=${CAOS_FLY_RAM_MB:-2048}
STD=${CAOS_FLY_STD:-all}
# Make flyctl act as the org token (not whatever interactive login is present),
# so apps land in the right org and this works headless.
export FLY_API_TOKEN=$CAOS_FLY_TOKEN

SERVER=$STACK-caos-server
REDIS=$STACK-caos-redis
WORKER_PREFIX=$STACK-caos-worker-

say() { echo "==> $*" >&2; }

# --- idempotency helpers (no jq dependency) ---
app_exists()  { flyctl status -a "$1" >/dev/null 2>&1; }
machine_id()  { flyctl machines list -a "$1" --json 2>/dev/null \
                  | grep -oE '"id": *"[0-9a-f]{14}"' | head -1 | grep -oE '[0-9a-f]{14}'; }
has_machine() { [ -n "$(machine_id "$1")" ]; }
has_volume()  { flyctl volumes list -a "$1" 2>/dev/null | grep -q 'caos_git'; }
has_ip()      { flyctl ips list -a "$1" 2>/dev/null | grep -qiE '[[:space:]]v[46][[:space:]]'; }

# 1. The three apps.
for app in "$SERVER" "$REDIS"; do
  if app_exists "$app"; then say "app $app exists"; else
    say "creating app $app (org $ORG)"; flyctl apps create "$app" --org "$ORG"
  fi
done

# Build the caosd image (cheap when cached) and decide whether anything actually
# changed. The image is content-addressed by its nix store path, so we sign
# (store path + the env we bake into the machine) and compare to a per-stack
# marker. An unchanged signature + a running machine means the deployed caosd is
# already this exact image+config — so we can skip the slow re-push and the
# machine restart entirely (the dominant cost of a no-op redeploy).
say "building caosd image (nix)"
if ! server_sp=$(nix build .#caos-server-docker --no-link --print-out-paths); then
  echo "caosd image build failed" >&2; exit 1
fi
marker=".caos-dev/deploy-cache/$STACK-server"
deploy_sig=$(printf '%s|%s|%s|%s|%s|%s|%s' \
  "$server_sp" "$RAM_MB" "$POOL" "$ORG" "$REGION" "$WORKER_PREFIX" "$CAOS_FLY_TOKEN" \
  | sha1sum | cut -c1-40)
caosd_unchanged=0
if [ "$(cat "$marker" 2>/dev/null)" = "$deploy_sig" ] && has_machine "$SERVER"; then
  caosd_unchanged=1
  say "caosd unchanged — skipping image push and machine update"
fi

# 2-4. Start redis and (push caosd image unless unchanged) + ensure the volume —
# in parallel. redis is a plain image reached over 6PN by name (no public
# service); independent of caosd. The caosd machine (step 5) only needs its image
# pushed and volume present, so we wait for just that job before it. (No separate
# registry app: worker images go straight into the server's own fly repo.)
start_redis() {
  has_machine "$REDIS" && { say "redis machine exists"; return; }
  say "starting redis"
  flyctl machine run redis:7 -a "$REDIS" -r "$REGION" --name redis --vm-memory 256
}
prep_caosd() {
  if [ "$caosd_unchanged" -eq 0 ]; then
    say "pushing caosd image to registry.fly.io/$SERVER:latest"
    nix shell nixpkgs#skopeo -c skopeo --insecure-policy copy \
      --dest-creds "x:$CAOS_FLY_TOKEN" \
      "docker-archive:$server_sp" "docker://registry.fly.io/$SERVER:latest"
  fi
  if has_volume "$SERVER"; then say "/git volume exists"; else
    say "creating /git volume (${VOL_GB}GB)"
    flyctl volumes create caos_git -a "$SERVER" -r "$REGION" -s "$VOL_GB" --yes
  fi
}

say "starting redis and prepping caosd — in parallel..."
start_redis & p_redis=$!
prep_caosd  & p_caosd=$!
# The caosd machine needs its image + volume; wait for that job (surface errors).
wait "$p_caosd" || { echo "caosd image/volume prep failed" >&2; exit 1; }

# 5. caosd machine: public ingress (80/443 -> internal 80), the volume, env, and
#    scale-to-zero. caosd binds [::]:80 and self-bootstraps /git on first boot.
if [ "$caosd_unchanged" -eq 1 ]; then
  say "caosd machine already current"
elif has_machine "$SERVER"; then
  say "updating caosd machine to the fresh image"
  flyctl machine update "$(machine_id "$SERVER")" -a "$SERVER" \
    --image "registry.fly.io/$SERVER:latest" --yes
else
  say "creating caosd machine"
  flyctl machine run "registry.fly.io/$SERVER:latest" -a "$SERVER" -r "$REGION" \
    --name caosd --vm-memory "$RAM_MB" --volume caos_git:/git \
    --port 80:80/tcp:http --port 443:80/tcp:http:tls \
    --autostop=stop --autostart \
    -e CAOS_BACKEND=fly \
    -e "CAOS_SERVER_URL=http://$SERVER.internal" \
    -e "CAOS_FLY_IMAGE_REPO=registry.fly.io/$SERVER" \
    -e "CAOS_REDIS_ADDR=$REDIS.internal:6379" \
    -e "CAOS_FLY_ORG=$ORG" -e "CAOS_FLY_REGION=$REGION" -e "CAOS_FLY_POOL=$POOL" \
    -e "CAOS_FLY_WORKER_PREFIX=$WORKER_PREFIX" \
    -e "CAOS_FLY_TOKEN=$CAOS_FLY_TOKEN"
fi
# Record the deployed signature so the next unchanged redeploy skips the above.
mkdir -p "$(dirname "$marker")" && printf '%s' "$deploy_sig" >"$marker"

# redis doesn't block caosd boot, but surface any failure before we go on.
wait "$p_redis" || { echo "redis start failed" >&2; exit 1; }

# 6. Public IPs so the CLI can reach caosd at https://$SERVER.fly.dev.
if has_ip "$SERVER"; then say "public IP exists"; else
  say "allocating public IPs"
  flyctl ips allocate-v4 --shared -a "$SERVER"
  flyctl ips allocate-v6 -a "$SERVER"
fi

# 7. Wait for caosd to answer (any HTTP reply — it 404s on /, which is fine).
say "waiting for caosd at http://$SERVER.fly.dev ..."
for _ in $(seq 1 60); do
  curl -s -o /dev/null --max-time 5 "http://$SERVER.fly.dev/" && break
  sleep 2
done

# 8. Publish the std library so the stack is immediately runnable. Builds each
#    builtin with the serve-capable caos and pushes them to refs/caos/std; `all`
#    includes the heavy rustc image (needs the 10GB volume / 2GB RAM defaults).
if [ "$STD" = none ]; then
  say "skipping std (CAOS_FLY_STD=none)"
else
  [ "$STD" = all ] && names=() || read -ra names <<<"$STD"
  say "publishing std: ${names[*]:-<all>}"
  CAOS_SERVER_URL="http://$SERVER.fly.dev" ./build-builtins.sh "${names[@]}" >/dev/null
fi

cat >&2 <<EOF

==> stack '$STACK' is up.
    server:    http://$SERVER.fly.dev   (scales to zero; wakes on request)
    workers:   ${WORKER_PREFIX}<hash16>   (created on demand, this stack only)
    std:       ${STD}$([ "$STD" = none ] && echo '   (none hosted — /cas/std/* unavailable until published)')

    smoke test:
      cd \$(mktemp -d) && git init -q && \\
        git remote add caos http://$SERVER.fly.dev && \\
        $PROJECT/result-caos/bin/caos-cli run /cas/std/hello out -- && cat out/receipt

    re-host the std library later (or a subset):
      CAOS_SERVER_URL=http://$SERVER.fly.dev ./build-builtins.sh [names...]

    tear the stack down (apps, machines, volume, IPs):
      ./teardown-fly.sh $STACK
EOF
