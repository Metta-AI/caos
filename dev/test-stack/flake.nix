{
  # dev/test-stack (SPEC, "Building and testing caos, including inside caos"): the
  # image a caos build or test runs IN. An ordinary worker image — runnerd
  # starts it, `/worker` runs the curried `worker1` — that happens to carry nix
  # and a stack's userland, and to declare the two grants that make that useful.
  #
  # WHY A WORKER AT ALL, rather than a container something launches on the side.
  # Being a worker is what makes the engine distribute it (content-addressed,
  # pulled on demand, nothing to `docker load` by hand), what makes the job
  # cache on its ArgTree like any other, and what hands its whole lifecycle —
  # naming, the crash reaper, removal — to runnerd, which already implements all
  # of it. An earlier cut launched this container from a script and had to
  # reimplement every one of those, badly.
  #
  # THE GRANTS, all declared here because an image is the only thing that
  # can declare them (runnerd reads its config env, never a job or its args):
  #
  #   CAOS_GRANT_VOLUMES        persistent storage. `/mounted-nix` is a nix store
  #                             this image binds over its own (see ./worker), so
  #                             a rebuild is incremental rather than from nothing;
  #                             `/caos-dev` is the dev stacks' git repo and their
  #                             per-run directories; `/caos-images` is their
  #                             podman store. All three hold content-addressed
  #                             data, which is why they are shared rather than
  #                             exclusive — several of these run at once against
  #                             one store and one object database.
  #
  #                             SHARED IS NOT UNSTRUCTURED. What is content-keyed
  #                             is shared outright; the name-keyed remainder — a
  #                             stack's logs, its client symlink, its publish
  #                             client repo, its seed ref — is per CONTAINER,
  #                             under `/caos-dev/runs/<id>` and reachable in here
  #                             as `/caos-run`. dev/stack-up's header says which
  #                             is which and what each one broke before it moved.
  #   CAOS_GRANT_SYS_ADMIN      that bind is the only way to replace a directory
  #                             whose replacement is on another filesystem.
  #   CAOS_GRANT_DEVICES        /dev/fuse, so the podman this image carries can
  #                             use fuse-overlayfs rather than falling back to
  #                             vfs, which copies a rootfs per container.
  #
  # NOT the engine socket, which this image used to need and no longer does: a
  # dev stack ran its workers as SIBLINGS on the host engine, which meant holding
  # a socket that is root-equivalent over it. It runs its own podman now.
  #
  # NOT redis, and not by omission. Two redis servers cannot share a directory:
  # both start, neither locks, and they interleave a single AOF between two
  # datasets that disagree (measured). So a dev stack points at an existing
  # redis — `CAOS_STACK_REDIS=no` — and what keeps its entries apart from
  # everyone else's is `CAOS_CACHE_NAMESPACE`, a prefix identifying the build,
  # rather than storage of its own.
  #
  # THE STORE IS SEEDED BY ./worker, NOT BY THE ENGINE. Mounting a fresh named
  # volume over a populated path WOULD make the engine copy the content in, but
  # that mechanism is deliberately not relied on: the volume mounts at a path of
  # its own and ./worker does the copy explicitly. `includeNixDB` is still on:
  # a nix build needs its closure REGISTERED in the db, not merely present in the
  # store, and this image's db is what ./worker merges into the volume's.
  #
  # THE USERLAND IS A STACK'S. `stack/serve` and `build-builtins.sh` run in here
  # when a test brings a stack up, and design/test-stack-image.md records the
  # cost of getting that wrong — "the image's userland is the publisher's
  # userland", learned by way of a missing `gawk`, since coreutils has no awk.
  # An absent binary here is exit 127 at runtime, not a build error.
  #
  # This directory IS the published tree (literal trees): flake.nix,
  # worker. The lock is NOT here: it is DEPped from the repo root and placed by
  # the flake-builder, so there is one lock in the tree rather than a copy per
  # flake and a lint to keep the copies honest.
  description = "caos dev/test-stack — the image a caos build or test runs in: nix, a stack userland, and the grants for both";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          workerRoot = pkgs.runCommand "dev-worker-root" { } ''
            mkdir -p $out
            install -m 755 ${./worker} $out/worker
          '';
        in
        pkgs.dockerTools.buildLayeredImage {
          name = "caos-test-stack";
          tag = "latest";
          # No Entrypoint: runnerd forces `/bin/caos runner`, which execs
          # /worker.
          contents = [
            workerRoot
            pkgs.nix
            # mount(8), for the bind that swaps the persistent store over this
            # image's own — see ./worker.
            pkgs.util-linux
            # THE ENGINE THIS STACK DISPATCHES ON. Its own, not the host's.
            #
            # `fuse-overlayfs` is the part that makes this affordable: podman
            # nested inside podman cannot use the kernel's overlay directly, and
            # its fallback (`vfs`) copies a whole rootfs per container. Measured
            # on a 120 MB image, three starts: 1.84s on vfs against 0.25s with
            # fuse-overlayfs — which is also faster than the outer engine's
            # 0.31s. Hence the /dev/fuse grant; without the device podman finds
            # this binary and then dies "device /dev/fuse not found".
            pkgs.podman
            pkgs.crun
            pkgs.fuse-overlayfs
            # podman shells out to these for image transport and networking.
            pkgs.iptables
            pkgs.shadow
            # nix fetches flake inputs in the EVALUATOR — this process, not a
            # daemon — so https here needs its own trust store.
            pkgs.cacert
            pkgs.bashInteractive
            pkgs.coreutils
            pkgs.gnugrep
            pkgs.gnused
            pkgs.findutils
            pkgs.diffutils
            pkgs.gnutar
            pkgs.gzip
            pkgs.curl
            pkgs.jq
            # build-builtins.sh reads a manifest digest out of `curl -I` with
            # awk, which coreutils does not have.
            pkgs.gawk
            pkgs.gitMinimal
            pkgs.skopeo
            # redis-cli, not redis-server: a dev stack runs no redis of its own,
            # but `stack/serve` probes the one it was pointed at.
            pkgs.redis
            (if pkgs.stdenv.hostPlatform.isLinux then
              pkgs.docker-client.override { buildxSupport = false; composeSupport = false; }
            else
              pkgs.docker-client)
          ];
          config = {
            Env = [
              "PATH=/bin"
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              "NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              # Single-user, unsandboxed nix: root in a container with no nixbld
              # group and no privilege to build a sandbox. The same three
              # settings std/flake-builder passes for the same situation, as
              # NIX_CONFIG so a script that simply says `nix build` inherits
              # them.
              ''NIX_CONFIG=experimental-features = nix-command flakes
build-users-group =
sandbox = false''
              # Root: the store is root-owned and the engine socket is the
              # user's, which container root maps to under rootless podman.
              "CAOS_WORKER_UID=0"
              "CAOS_WORKER_GID=0"
              # NOT the engine socket. A dev stack used to delegate its workers
              # to the OUTER engine as siblings, which meant handing this image a
              # socket that is root-equivalent over the host's engine. It runs
              # its own podman now, so the most dangerous grant in the tree is
              # simply not asked for.
              "CAOS_GRANT_SYS_ADMIN=1"
              "CAOS_GRANT_DEVICES=/dev/fuse"
              "CAOS_GRANT_VOLUMES=/mounted-nix /caos-dev /caos-images"
            ];
          };
          # /usr/bin/env, because nearly every script in this tree opens with
          # `#!/usr/bin/env bash` and running them is what this image is for.
          # A worker image normally gets it from the caos additions the
          # flake-builder stacks on; carrying it costs nothing and removes the
          # dependency.
          #
          # /tmp because a bare nix root has none and a worker scratches there.
          #
          # THE REST IS WHAT PODMAN NEEDS TO EXIST AT ALL, and every line of it
          # was a distinct failure before it was a line here:
          #
          #   /etc/passwd, /etc/group   "unable to resolve HOME directory:
          #                             user: unknown userid 0"
          #   policy.json               "no policy.json file found at any of…"
          #   registries.conf           an unqualified image name resolves
          #                             nowhere without a search registry
          #   /var/tmp                  "creating a temporary directory:
          #                             stat /var/tmp: no such file or directory"
          #
          # cgroups = "disabled" and cgroup_manager = "cgroupfs" belong in
          # containers.conf rather than on every `run`: /sys/fs/cgroup is
          # read-only in here, so crun cannot write cgroup.subtree_control and
          # dies enabling controllers. Config, not flags, because runnerd builds
          # its own `run` command line and has nowhere to put extra ones.
          fakeRootCommands = ''
            mkdir -p usr/bin
            ln -s /bin/env usr/bin/env
            mkdir -p tmp
            chmod 1777 tmp
            mkdir -p var/tmp
            chmod 1777 var/tmp
            mkdir -p root etc/containers
            printf 'root:x:0:0:root:/root:/bin/bash\n' > etc/passwd
            printf 'root:x:0:\n' > etc/group
            printf '{"default":[{"type":"insecureAcceptAnything"}]}\n' \
              > etc/containers/policy.json
            printf 'unqualified-search-registries=["docker.io"]\n[[registry]]\nlocation = "caos-registry:5000"\ninsecure = true\n[[registry]]\nlocation = "localhost:5000"\ninsecure = true\n' \
              > etc/containers/registries.conf
            printf '[containers]\ncgroups = "disabled"\n[engine]\ncgroup_manager = "cgroupfs"\n' \
              > etc/containers/containers.conf
            printf '[storage]\ndriver = "overlay"\n[storage.options.overlay]\nmount_program = "/bin/fuse-overlayfs"\n' \
              > etc/containers/storage.conf
          '';
          includeNixDB = true;
        };
    in
    {
      packages = builtins.listToAttrs (
        map
          (system: {
            name = system;
            value = {
              caosImage = forSystem system;
            };
          })
          [
            "x86_64-linux"
            "aarch64-linux"
          ]
      );
    };
}
