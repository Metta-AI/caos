# Tiltfile — local dev for caos. Builds the images with Nix (only when their
# sources change) and runs the daemons. Start with `tilt up`, stop with
# `tilt down`. Open the UI at http://localhost:10350.
#
# Everything below shells out to `docker` (which may be Podman) via Tilt's
# local_resource, so no Tilt docker/Kubernetes integration is required.

NET = 'caos-net'

# The caos server owns a *dedicated* bare repo — not the project's own `.git`.
# Clients never touch it directly; they reach it only through the server, using
# it as the `caos` git remote: `git push` objects up, `git fetch` refs/results
# back down (over the smart-HTTP transport the server now speaks). It lives under
# the gitignored .caos-dev/ so it's easy to inspect and survives `tilt down`;
# `setup` (below) creates it, idempotently. Computed at Tiltfile load
# (cwd = this directory) so it doesn't depend on `$PWD`.
SERVER_REPO = os.path.abspath('.caos-dev/server-repo.git')

# Per-image marker files. Each image build bumps its marker once the new image is
# loaded; a daemon depends on its image's marker (see `daemon` below), so loading
# a fresh image restarts the daemon onto it. Kept under the gitignored .caos-dev/.
MARKERS = os.path.abspath('.caos-dev/markers')

# Files that affect every image: the flake and the locked workspace.
COMMON = [
    'flake.nix',
    'flake.lock',
    'Cargo.toml',
    'Cargo.lock',
    'rust-toolchain.toml',
]

# Build an image with Nix and load it into the local engine, then bump its marker
# so any daemon running it restarts onto the fresh image. Tilt re-runs this only
# when one of `deps` changes, so unchanged images are not rebuilt or reloaded on
# every `tilt up` — that is what made dev-up.sh slow.
def nix_image(res, app, srcs):
    local_resource(
        res,
        cmd='nix run .#%s && mkdir -p %s && date +%%s%%N > %s/%s' % (app, MARKERS, MARKERS, res),
        deps=COMMON + srcs,
        labels=['images'],
    )

nix_image('img-server', 'load-caos-server', ['crates/server'])
nix_image('img-worker-base', 'load-caos-worker-base', ['crates/caos'])
nix_image('img-worker-hello', 'load-caos-worker-hello', ['crates/caos'])
nix_image('img-worker-fold', 'load-caos-worker-fold', ['crates/caos'])
nix_image('img-worker-file-count', 'load-caos-worker-file-count', ['crates/caos'])
nix_image('img-worker-deep-deps', 'load-caos-worker-deep-deps', ['crates/caos'])

# One-time infra: the docker network the daemons and the worker containers the
# server spawns all share, plus the server's own bare repo (see SERVER_REPO).
local_resource(
    'setup',
    # Create the markers dir, the shared docker network, and the server's own bare
    # repo. `http.receivepack=true` lets clients `git push` to it over smart-HTTP;
    # `uploadpack.allowAnySHA1InWant=true` lets them `git fetch` a computation
    # result by its bare hash (results live unreferenced in the repo). `git init
    # --bare` is idempotent, so re-running setup is safe and leaves an existing
    # repo (and its pushed objects/refs) untouched.
    cmd=' && '.join([
        'mkdir -p %s' % MARKERS,
        '(docker network create %s >/dev/null 2>&1 || true)' % NET,
        'git init -q --bare %s' % SERVER_REPO,
        'git -C %s config http.receivepack true' % SERVER_REPO,
        'git -C %s config uploadpack.allowAnySHA1InWant true' % SERVER_REPO,
    ]),
    labels=['infra'],
)

# Run a daemon as a foreground container Tilt supervises. It depends on its
# image's marker file, so rebuilding+reloading that image (which bumps the marker)
# restarts the container onto the fresh image; `resource_deps` orders it after
# setup and the image build. Depending on the marker rather than the crate sources
# directly is deliberate: it restarts *after* the new image is loaded, not racing
# the rebuild and coming up on the stale image.
#
# Teardown: the daemons install a SIGINT/SIGTERM handler (see their main.rs), so
# `exec docker run` forwards Tilt's signal to the container and it exits promptly,
# `--rm` cleaning it up. The leading `docker rm -f` reclaims any stale container
# from a prior run that was hard-killed (SIGKILL) before it could exit.
def daemon(name, run_args, extra_deps=[]):
    img = 'img-' + name.replace('caos-', '')
    local_resource(
        name,
        serve_cmd=' '.join(
            ['docker rm -f %s >/dev/null 2>&1;' % name,
             'exec docker run --rm --name %s --network %s' % (name, NET)]
            + run_args
            + ['%s:latest' % name]
        ),
        deps=['%s/%s' % (MARKERS, img)],
        resource_deps=['setup', img] + extra_deps,
        labels=['daemons'],
    )

# Result cache. Stock image, no persistence yet. It handles SIGTERM itself, so a
# foreground `exec docker run --rm` tears down cleanly on Ctrl-C.
local_resource(
    'caos-redis',
    serve_cmd='docker rm -f caos-redis >/dev/null 2>&1; ' +
              'exec docker run --rm --name caos-redis --network %s -p 6379:6379 redis:7' % NET,
    resource_deps=['setup'],
    labels=['daemons'],
)

# Docker registry the server pushes converted git images to. The compute
# server reaches it by name on caos-net (caos-registry:5000); the host docker
# daemon, which runs the workers, pulls via the published localhost:5000 (which
# docker treats as an insecure registry, so no TLS config is needed). Stock
# image; handles SIGTERM itself.
local_resource(
    'caos-registry',
    serve_cmd='docker rm -f caos-registry >/dev/null 2>&1; ' +
              'exec docker run --rm --name caos-registry --network %s -p 5000:5000 registry:2' % NET,
    resource_deps=['setup'],
    labels=['daemons'],
)

# The one caos server: storage + compute in a single process. It serves /object
# and the git smart-HTTP transport from its own bare repo (mounted at /git), and
# /run by spawning worker containers (hence the docker socket), so it needs all
# of: the repo mount, the docker socket, the network, the cache, and the registry.
daemon(
    'caos-server',
    [
        '-p 9090:80',
        '-e CAOS_DOCKER_NETWORK=%s' % NET,
        '-v /var/run/docker.sock:/var/run/docker.sock',
        '-v "%s:/git"' % SERVER_REPO,
    ],
    # Uses the cache and pushes converted images to the registry; start it after
    # both. (It degrades gracefully if Redis is down; the registry is only needed
    # when running a git image.)
    extra_deps=['caos-redis', 'caos-registry'],
)
