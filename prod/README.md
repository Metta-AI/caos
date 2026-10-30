# prod — the machines that run caos

A machine here is a **role**, named `<daemon>-<environment>`, defined as a
`nixosConfigurations` attribute in the root `flake.nix`. Today there is one:

| role | module | what it runs |
|---|---|---|
| `caosd-prod` | `caosd/configuration.nix` | `caosd up --iroh` as a systemd unit |
| `watchdog-prod` | `watchdog/configuration.nix` | `caos-watchdog`: probes caosd-prod, responds to its Datadog alerts |

Both import `base.nix`: the EC2 base (SSM shell, amazon-init off, the dhcpcd
guard), secrets from AWS Secrets Manager, and the Datadog agent.

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
It also comes back when the stack's container dies: the unit waits on `caos-stack`
and fails when it exits, and `Restart=always` runs `caosd up` again 30 seconds
later. `journalctl -u caosd` shows "the caos-stack container exited".

```sh
systemctl status caosd          # the stack
journalctl -u caosd             # why it did not start
sudo cat /data/caos/stack/iroh/ticket
```

The ticket is the one string a client needs, and it survives restarts:

```sh
git remote add caos caos://<ticket>
```

## Tuning redis memory

Redis is capped at 2gb unless the machine says otherwise, and at the cap it
evicts live results and blobs (`allkeys-lru`) rather than failing. Set the cap
in a file on the box; no deploy is needed, only a restart:

```sh
echo 16gb | sudo -u caos tee /data/caos/redis-maxmemory
sudo systemctl restart caosd
```

The file overrides the default for the `caosd` unit; delete it to go back to
2gb. Check with `docker exec caos-stack redis-cli info memory` (`maxmemory_human`,
`evicted_keys`).

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

## The watchdog

`watchdog-prod` is a t3.small in the same account that shares none of the
server's failure modes: no docker, no stack, no `/data`. It exists because the
failures that matter most (a container veth swallowing the metadata route and
killing SSM; disk or memory exhaustion) are exactly the ones a process on the
server cannot report.

Detection is Datadog's. The agent on each host reports units, disk, memory
and containers; `caos-watchdog probe` runs every two minutes and adds three
service checks: `caos.probe.iroh` (a real `git ls-remote caos://<ticket>`),
`caos.probe.run` (`caos-cli run std/hello` with a fresh `--date`, so the
server has to compute, not just answer) and `caos.probe.ssm` (an SSM
RunCommand round trip). The two client latencies go up as gauges,
`caos.probe.iroh.seconds` and `caos.probe.run.seconds`. The monitors over
all of it are code in metta, `devops/tf/datadog-monitors/caos_monitors.tf`,
tagged `service:caos`.

Response is `caos-watchdog respond`, a daemon that polls those monitors.
When any alert, it posts to Slack #proj-caos at once (no model in that path),
waits 90 s for the rest of the storm, then runs ONE Claude Code session over
the whole set with `watchdog/runbook/CLAUDE.md` as its instructions. The
session reaches the host only through `caos-watchdog ssh`, which refuses
reboots, redeploys and anything that rewrites `/data/caos/stack`; those it
asks for in Slack. `watchdog/respond.go` has the caps: one incident at a
time, a session only when something NEW alerts, at most 3 per incident and 8
per day. The ledger is `/var/lib/caos-watchdog/incident.json` and
`incidents.jsonl`; transcripts are under `sessions/`.

Deploying it:

```sh
sudo nixos-rebuild switch --refresh --flake github:Metta-AI/caos#watchdog-prod
```

The instance, its IAM role (SSM on the one prod instance, read of four
secrets, nothing that reboots) and the empty secrets are OpenTofu in metta,
`devops/tf/caos`. The secret VALUES are set by hand once:

```sh
aws secretsmanager put-secret-value --profile caos-admin --region us-east-1 \
  --secret-id caos/datadog-api-key --secret-string "$DD_API_KEY"
# likewise caos/watchdog/datadog-app-key, caos/watchdog/anthropic-api-key,
# caos/watchdog/slack-webhook
```

From a shell on the box: `caos-watchdog ssh -- <cmd>` runs a command on the
server, `systemctl status caos-watchdog caos-watchdog-probe.timer` shows the
two units, `journalctl -u caos-watchdog-probe` the probe results.

## The binary cache

Hosts never build caos. `s3://caos-nix-cache-109251245574` is a private
binary cache in the account; both roles list it as a substituter with its
public key (`prod/base.nix`) and read it through their instance role. A
deploy therefore builds elsewhere and pushes first:

```sh
KEY=$(mktemp) && aws secretsmanager get-secret-value --profile caos-admin \
  --region us-east-1 --secret-id caos/nix-cache-signing-key \
  --query SecretString --output text > "$KEY"
AWS_PROFILE=caos-admin nix copy \
  --to "s3://caos-nix-cache-109251245574?region=us-east-1&secret-key=$KEY" \
  .#nixosConfigurations.watchdog-prod.config.system.build.toplevel
rm "$KEY"
```

Then on the host, `nixos-rebuild switch --refresh --flake
github:Metta-AI/caos#watchdog-prod` is substitution, not compilation. Push
`.#caos-tools` the same way before `deploy-caosd-prod` and the server's
deploy stops rebuilding 498 crates.

## Adding a role

Add `prod/<daemon>/configuration.nix` and a `nixosConfigurations.<daemon>-<env>`
entry in the root `flake.nix`, merged **outside** `flake-utils.lib.eachDefaultSystem`
— `nixosConfigurations` is not per-system. Take the daemon from
`self.packages.x86_64-linux.*` so the machine and the checkout are one revision.

Import `base.nix` for the EC2 base, secrets and Datadog; add the role's
units to `caos.datadog.units` so the shared unit monitor covers them.
