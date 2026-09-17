# prod — the machines that run caos

A machine here is a **role**, named `<daemon>-<environment>`, defined as a
`nixosConfigurations` attribute in the root `flake.nix`. Today there is one:

| role | module | what it runs |
|---|---|---|
| `caosd-prod` | `caosd/configuration.nix` | `caosd up --iroh` as a systemd unit |

## Deploying

On the machine you want to become the host:

```sh
nix run github:Metta-AI/caos#deploy-caosd-prod
```

That is the whole procedure. It reads the host's facts, refuses if the machine
is not ready, builds the role with those facts, and switches. Add `--refresh`
if nix serves a stale revision, and pass a flake ref to deploy from somewhere
other than the revision the driver itself came from:

```sh
nix run 'git+https://github.com/Metta-AI/caos?ref=my-branch#deploy-caosd-prod'
```

The driver bakes in its own source (`-X main.defaultFlake=path:${self}`), so
the driver and the host config it applies are always the same revision. Passing
a ref means only "build from somewhere else" — it is not needed to make a
branch deploy itself.

### Do not use `nixos-rebuild` directly

```sh
nixos-rebuild switch --flake github:Metta-AI/caos#caosd-prod   # FAILS
# error: The option `caos.advertiseAddress' was accessed but has no value defined
```

That is deliberate. The address clients reach the machine on differs per box,
so a role several machines share cannot declare it, and a pure flake cannot
read it. The driver reads it from IMDS and layers it on with `extendModules`.
An unset option is a named build failure rather than a machine that quietly
advertises a private address and makes every client fall back to an iroh relay
(~4.6s versus ~100ms for a cached run).

## What the machine needs first

* **A NixOS EC2 instance.** Any recent NixOS AMI.
* **A data volume labelled `caos-data`.** It carries docker's `data-root`, the
  registry, redis and the server's git repo; without it they land on the small
  root disk. Label it once — this **erases** that disk:

  ```sh
  sudo mkfs.ext4 -L caos-data /dev/nvme1n1
  ```

  To adopt a volume that already holds a filesystem, relabel instead:
  `sudo e2label /dev/nvme1n1 caos-data`.
* **A public IPv4, ideally an Elastic IP.** It goes into the iroh ticket, so it
  should not change under clients. The driver refuses if there is none.
* **Inbound UDP 11204**, the iroh transport's fixed port. Nothing else needs to
  be open: there is no sshd, and shell access is over SSM.

## After deploying

`caosd` is a systemd unit, enabled, so it comes back on its own after a reboot.

```sh
systemctl status caosd          # the stack
journalctl -u caosd             # why it did not start
sudo cat /data/caos/stack/iroh/ticket
```

The ticket is the one string a client needs, and it survives restarts:

```sh
git remote add caos caos://<ticket>
```

## Migrating a machine that already ran caos by hand

Wipe the stack state; do **not** chown it.

```sh
sudo systemctl stop caosd
sudo rm -rf /data/caos/stack
sudo systemctl start caosd
```

The state tree is deliberately mixed-ownership — the top level belongs to the
host user running caosd, while `git/`, `redis/` and `registry/` are created by
the stack container as root. A blanket `chown -R` looks like the fix and breaks
the server, which then reports the repo as `fatal: not in a git directory`.

## Adding a role

Add `prod/<daemon>/configuration.nix` and a `nixosConfigurations.<daemon>-<env>`
entry in the root `flake.nix`, merged **outside** `flake-utils.lib.eachDefaultSystem`
— `nixosConfigurations` is not per-system. Take the daemon from
`self.packages.x86_64-linux.*` so the machine and the checkout are one revision.

The EC2 base, the dhcpcd guard and the docker address-pool move in
`caosd/configuration.nix` are all things a second host will want; extract them
into a shared module when that host exists and shows which parts are genuinely
common.
