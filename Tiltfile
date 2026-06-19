# Tiltfile — local dev for caos. Builds the images with Nix (only when their
# sources change) and runs the daemons. Start with `tilt up`, stop with
# `tilt down`. Open the UI at http://localhost:10350.
#
# Everything below shells out to `docker` (which may be Podman) via Tilt's
# local_resource, so no Tilt docker/Kubernetes integration is required.

NET = 'caos-net'

# Absolute path to a throwaway git repo for the object server. Computed here (at
# Tiltfile load, with cwd = this directory) so it doesn't depend on `$PWD` or the
# working directory the serve command happens to run in.
GIT_REPO = os.path.abspath('.caos-dev/git')

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

nix_image('img-object-server', 'load-caos-object-server', ['crates/object-server'])
nix_image('img-compute-server', 'load-caos-compute-server', ['crates/compute-server'])
nix_image('img-worker-base', 'load-caos-worker-base', ['crates/client'])
nix_image('img-worker-bash', 'load-caos-worker-bash', ['crates/client'])
nix_image('img-worker-hello', 'load-caos-worker-hello', ['crates/client'])
nix_image('img-worker-fold', 'load-caos-worker-fold', ['crates/client'])
nix_image('img-worker-file-count', 'load-caos-worker-file-count', ['crates/client'])

# One-time infra: the docker network the daemons and the worker containers the
# compute server spawns all share, plus a git repo for the object server to
# store objects in (kept under the gitignored .caos-dev/).
local_resource(
    'setup',
    # `git init` is idempotent, so run it unconditionally. (Don't probe with
    # `git rev-parse` first: GIT_REPO sits inside this project's own repo, so the
    # probe would discover the *parent* repo and wrongly skip init, leaving /git
    # without a .git of its own.)
    cmd=' && '.join([
        'mkdir -p %s' % GIT_REPO,
        'mkdir -p %s' % MARKERS,
        'git init -q %s' % GIT_REPO,
        '(docker network create %s >/dev/null 2>&1 || true)' % NET,
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

daemon(
    'caos-object-server',
    ['-p 8080:80', '-v "%s:/git"' % GIT_REPO],
)
daemon(
    'caos-compute-server',
    [
        '-p 9090:80',
        '-e CAOS_DOCKER_NETWORK=%s' % NET,
        '-v /var/run/docker.sock:/var/run/docker.sock',
    ],
    # Compute server reads/writes the cache; start it after Redis. (It degrades
    # gracefully if Redis is down, so this is just for tidy startup.)
    extra_deps=['caos-redis'],
)
