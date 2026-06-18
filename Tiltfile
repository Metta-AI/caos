# Tiltfile — local dev for caos. Builds the images with Nix (only when their
# sources change) and runs the daemons. Start with `tilt up`, stop with
# `tilt down`. Open the UI at http://localhost:10350.
#
# Everything below shells out to `docker` (which may be Podman) via Tilt's
# local_resource, so no Tilt docker/Kubernetes integration is required.

NET = 'caos-net'

# Files that affect every image: the flake and the locked workspace.
COMMON = [
    'flake.nix',
    'flake.lock',
    'Cargo.toml',
    'Cargo.lock',
    'rust-toolchain.toml',
]

# Build an image with Nix and load it into the local engine. Tilt re-runs this
# only when one of `deps` changes, so unchanged images are not rebuilt or
# reloaded on every `tilt up` — that is what made dev-up.sh slow.
def nix_image(res, app, srcs):
    local_resource(
        res,
        cmd='nix run .#%s' % app,
        deps=COMMON + srcs,
        labels=['images'],
    )

nix_image('img-object-server', 'load-caos-object-server', ['crates/object-server'])
nix_image('img-compute-server', 'load-caos-compute-server', ['crates/compute-server'])
nix_image('img-worker-base', 'load-caos-worker-base', ['crates/client'])
nix_image('img-worker-bash', 'load-caos-worker-bash', ['crates/client'])
nix_image('img-worker-hello', 'load-caos-worker-hello', ['crates/client'])

# One-time infra: the docker network the daemons and the worker containers the
# compute server spawns all share, plus a git repo for the object server to
# store objects in (kept under the gitignored .caos-dev/).
local_resource(
    'setup',
    cmd=' && '.join([
        'docker network create %s >/dev/null 2>&1 || true' % NET,
        'mkdir -p .caos-dev/git',
        'git -C .caos-dev/git rev-parse --git-dir >/dev/null 2>&1 ' +
        '|| git init -q .caos-dev/git',
    ]),
    labels=['infra'],
)

# Run a daemon as a foreground container Tilt supervises. `deps` restart it when
# its sources change; `resource_deps` order it after setup and its image build,
# so a restart always picks up the freshly loaded image.
def daemon(name, srcs, run_args):
    local_resource(
        name,
        serve_cmd=' '.join(
            ['docker rm -f %s >/dev/null 2>&1;' % name,
             'exec docker run --rm --name %s --network %s' % (name, NET)]
            + run_args
            + ['%s:latest' % name]
        ),
        deps=COMMON + srcs,
        resource_deps=['setup', 'img-' + name.replace('caos-', '')],
        labels=['daemons'],
    )

daemon(
    'caos-object-server',
    ['crates/object-server'],
    ['-p 8080:8080', '-v "$PWD/.caos-dev/git:/git"'],
)
daemon(
    'caos-compute-server',
    ['crates/compute-server'],
    [
        '-p 9090:9090',
        '-e CAOS_DOCKER_NETWORK=%s' % NET,
        '-v /var/run/docker.sock:/var/run/docker.sock',
    ],
)

# Redis will slot in here later, e.g.:
# local_resource(
#     'redis',
#     serve_cmd='docker rm -f redis >/dev/null 2>&1; ' +
#               'exec docker run --rm --name redis --network %s -p 6379:6379 redis:7' % NET,
#     resource_deps=['setup'],
#     labels=['daemons'],
# )
