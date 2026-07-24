#!/usr/bin/env bash
# The flake-builder (design/flake-images.md): the SOLE bootstrap builtin. One
# self-contained image — nix + skopeo + jq + caos + the runner trampoline — and
# one script that turns a flake dir into a runnable worker image, branching on a
# curried-in --stage:
#
#   orchestrate (default): run-then the BUILD stage on the flake tree, then the
#     STACK stage — a tail call the server resolves.
#   build: `nix build` the flake's #caosImage, stream it to the registry, and
#     return the CLEAN image as {ref, config} — no caos in it. Keyed on the
#     flake tree alone (registry tag flake-<H>), so a caos change never rebuilds
#     it (see design/flake-images.md).
#   stack: emit a git-docker delta {base: docker://<clean>, config.json,
#     layer00: /bin/caos setuid + /worker trampoline + /tmp + userdb}. The
#     server converts that (one caos layer on a docker base) into a digest.
#
# A flake's #caosImage is a BASE (no /worker): the delta supplies the runner
# trampoline, and a worker is curry(<flake image>, bin=<binary>) — the
# runner-pool model. So the flake tree is pure (toolchain/deps only), keyed
# without caos; only this cheap stack re-runs when caos changes.
set -euo pipefail

# skopeo (and nix) resolve $HOME; the worker runs as root in a minimal container
# where a uid-0 lookup can fail, so set it to the world-writable /tmp it owns.
export HOME=/tmp

fail() { echo "FLAKE BUILDER FAIL: $*" >&2; exit 1; }

# Args are lazy placeholders — fetch before reading. The initial (orchestrate)
# invocation carries no --stage, so the fetch fails and we default to it.
stage=orchestrate
if caos get /cas/args/stage 2>/dev/null; then stage=$(cat /cas/args/stage); fi

case "$stage" in

orchestrate)
  build=$(caos curry /cas/std/flake-builder -- --stage=build) || fail "currying build"
  stack=$(caos curry /cas/std/flake-builder -- --stage=stack) || fail "currying stack"
  caos run-then /cas/args/in -- --run="$build" --then="$stack"
  ;;

build)
  caos get -r /cas/args/in
  [ -e /cas/args/in/flake.nix ] || fail "no flake.nix in the flake tree"
  [ -e /cas/args/in/flake.lock ] || fail "no flake.lock in the flake tree"
  mkdir /tmp/ws
  cp -rL /cas/args/in/. /tmp/ws/

  H=$(caos hash /cas/args/in) || fail "hashing the flake tree"
  tag="caos-registry:5000/caos:flake-$H"
  # Single-user, unsandboxed nix: root in a container with no nixbld group and
  # no privilege to build a sandbox.
  nixf() {
    nix --extra-experimental-features "nix-command flakes" \
      --option build-users-group "" --option sandbox false "$@"
  }
  # Unsandboxed builders run with HOME=/homeless-shelter; a builder that
  # writes $HOME (go builds do — e.g. a flake compiling the slimmed docker
  # client) leaves that directory behind, and nix (2.35) then refuses to
  # START the next local build ("home directory exists ... purity") without
  # ever cleaning it up itself — post-build-hook demonstrably does not fire
  # for this. So: retry, removing the litter, but ONLY on that exact error —
  # completed drvs stay in the store, so each retry makes monotonic progress
  # and a genuinely broken flake still fails on its first real error.
  nix_build_flake() {
    while :; do
      rm -rf /homeless-shelter
      # The pipeline (pipefail is on) keeps the full log flowing to stderr
      # while capturing a copy to grep; `-o` carries the result, stdout is
      # empty.
      if nixf build -L "path:/tmp/ws#caosImage" -o /tmp/img 2>&1 | tee /tmp/nix-err >&2; then
        return 0
      fi
      grep -q "homeless-shelter" /tmp/nix-err || return 1
      echo "flake-build: homeless-shelter litter; resuming the build" >&2
    done
  }
  sk() { skopeo --insecure-policy "$@"; }

  # Already built for this exact flake tree? (The tag is the content hash.)
  if digest=$(sk inspect --tls-verify=false --format '{{.Digest}}' "docker://$tag" 2>/dev/null); then
    echo "flake-build: registry hit for $tag" >&2
  else
    nix_build_flake \
      || fail "nix build of path:/tmp/ws#caosImage (does the flake expose packages.<system>.caosImage?)"
    gunzip -c "$(readlink -f /tmp/img)" > /tmp/img.tar
    sk copy --dest-tls-verify=false "docker-archive:/tmp/img.tar" "docker://$tag" >&2 \
      || fail "push to the registry"
    digest=$(sk inspect --tls-verify=false --format '{{.Digest}}' "docker://$tag") \
      || fail "reading the pushed digest"
  fi

  mkdir /tmp/out
  # The on-network registry name: this ref becomes the delta's `base`, which the
  # SERVER pulls at convert time — inside the server container the registry is
  # caos-registry:5000, not the host-published localhost:5000.
  printf 'caos-registry:5000/caos@%s' "$digest" > /tmp/out/ref
  # The stacked image's config.json: the flake image's own OCI config with the
  # runner entrypoint forced and /bin appended to PATH (where the stacked setuid
  # caos lives). The clean image and its tag are untouched, so this does not
  # perturb the flake-keyed memo above.
  sk inspect --tls-verify=false --config "docker://$tag" > /tmp/cfg.json \
    || fail "reading the built image config"
  jq '
    (.config.Env // []) as $env
    | ($env | map(select(startswith("PATH="))) | .[0] // "PATH=/usr/bin:/bin") as $path
    | .config.Entrypoint = ["/bin/caos", "runner"]
    | .config.Env = (($env | map(select(startswith("PATH=") | not))) + [$path + ":/bin"])
  ' /tmp/cfg.json > /tmp/out/config \
    || fail "rewriting the image config"
  caos put /tmp/out /cas/out
  ;;

stack)
  caos get -r /cas/args/result
  clean=$(cat /cas/args/result/ref) || fail "no clean ref in the build result"
  [ -s /cas/args/result/config ] || fail "no config in the build result"

  img=/tmp/img
  l=$img/layer00
  mkdir -p "$l/usr/bin" "$l/bin" "$l/tmp" "$l/etc"

  # The setuid caos runner gateway. Git trees can't encode setuid, so a
  # `<name>.caosmeta` sidecar beside the entry carries mode/uid/gid; the server
  # applies it when it rebuilds the layer tar.
  cp /bin/caos "$l/usr/bin/caos"
  printf '{"mode":"4755","uid":0,"gid":0}' > "$l/usr/bin/caos.caosmeta"
  # A scratch/bare flake base has no /bin merge, and runnerd forces
  # --entrypoint /bin/caos — so create the link explicitly.
  ln -s /usr/bin/caos "$l/bin/caos"
  # The runner trampoline (baked into THIS image at /caos-trampoline): caos
  # runner execs /worker, which reads the `bin` arg and execs it. This is what
  # lets a flake #caosImage stay a pure base — the trampoline rides the caos
  # delta, not the flake.
  cp /caos-trampoline "$l/worker"
  chmod 0755 "$l/worker"
  # /usr/bin/env, for env-shebang worker scripts (images/bash-worker.sh runs
  # as a curried bin on flake bases) — the flake base's own /bin/env is where
  # coreutils puts it.
  ln -s /bin/env "$l/usr/bin/env"
  # The world-writable /tmp the unprivileged worker scratches in (empty dir
  # stores as an empty tree; its 1777 mode rides in the sibling sidecar).
  printf '{"mode":"1777","uid":0,"gid":0}' > "$l/tmp.caosmeta"
  # The user db (the runner drops to uid 1000 unless the image grants root).
  printf 'root:x:0:0:root:/root:/sbin/nologin\nworker:x:1000:1000:caos worker:/tmp:/sbin/nologin\n' > "$l/etc/passwd"
  printf 'root:x:0:\nworker:x:1000:\n' > "$l/etc/group"

  printf 'docker://%s' "$clean" > "$img/base"
  cp /cas/args/result/config "$img/config.json"
  caos put "$img" /cas/out
  ;;

*)
  fail "unknown --stage: $stage"
  ;;
esac
