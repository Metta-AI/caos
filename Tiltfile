# Tiltfile — local dev for caos. Builds the images with Nix (only when their
# sources change) and runs the daemons. Start with `tilt up`, stop with
# `tilt down`. Open the UI at http://localhost:10350.
#
# Everything below shells out to `docker` (which may be Podman) via Tilt's
# local_resource, so no Tilt docker/Kubernetes integration is required.

NET = 'caos-net'

# The object server is backed by *this project's own* `.git`, so caos can fetch
# real repo objects (run `git rev-parse HEAD:README.md` for a blob hash, or any
# tree hash) instead of an empty throwaway repo — more interesting test data.
# Computed at Tiltfile load (cwd = this directory) so it doesn't depend on `$PWD`.
# Note: the mount is read-write, so `caos put`/`caos run` write their objects
# (args trees, results) here as loose, unreferenced objects — harmless, and
# reclaimed by `git gc`.
GIT_REPO = os.path.abspath('.git')

# A checkout often keeps most of its objects in a *shared* store, referenced from
# .git/objects/info/alternates (here, a sibling bare repo). gix follows that by
# its absolute path, so the object server must see it at the *same* path inside
# the container — otherwise every object that lives only in the alternate 404s.
# Mount .git at /git, plus each alternate object dir at its own absolute path
# (read-only: git never writes to an alternate). Without this, only the handful
# of loose objects in this .git would resolve.
def object_server_mounts():
    mounts = ['-v "%s:/git"' % GIT_REPO]
    alternates = os.path.join(GIT_REPO, 'objects', 'info', 'alternates')
    if os.path.exists(alternates):
        for line in str(read_file(alternates)).splitlines():
            alt = line.strip().replace('//', '/')
            if alt and not alt.startswith('#'):
                mounts.append('-v "%s:%s:ro"' % (alt, alt))
    return mounts

OBJECT_SERVER_MOUNTS = object_server_mounts()

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
nix_image('img-worker-deep-deps', 'load-caos-worker-deep-deps', ['crates/client'])

# One-time infra: the docker network the daemons and the worker containers the
# compute server spawns all share. (The object server's git repo is this
# project's own .git — see GIT_REPO above — so there's nothing to create here.)
local_resource(
    'setup',
    # GIT_REPO is this project's existing .git, so there's nothing to init — just
    # the markers dir and the shared docker network.
    cmd=' && '.join([
        'mkdir -p %s' % MARKERS,
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

# Docker registry the compute server pushes converted git images to. The compute
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

daemon(
    'caos-object-server',
    ['-p 8080:80'] + OBJECT_SERVER_MOUNTS,
)
daemon(
    'caos-compute-server',
    [
        '-p 9090:80',
        '-e CAOS_DOCKER_NETWORK=%s' % NET,
        '-v /var/run/docker.sock:/var/run/docker.sock',
    ],
    # Compute server uses the cache and pushes converted images to the registry;
    # start it after both. (It degrades gracefully if Redis is down; the registry
    # is only needed when running a git image.)
    extra_deps=['caos-redis', 'caos-registry'],
)
