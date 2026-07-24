# Building on macOS

CAOS clients (`caos` and `caos-cli`) run natively on Darwin. The server,
runner, and workers run in Linux containers.

We recommend configuring a macOS Nix daemon and registering a remote builder
that advertises `aarch64-linux`. The daemon delegates build work and
consolidates output artifacts at `/nix/store`:

- Local Nix builder: builds `caos`, `caos-cli`, and `caosd`
- Remote Nix builder: builds Linux images and worker artifacts

`caosd` provides management conveniences for running the Linux containers
(server, runnerd, Redis, registry, and workers) in your Docker engine.

With this setup, a common build and usage pattern is:

```bash
nix build  # produces result/bin/{caos,caos-cli,caosd}
./result/bin/caosd up|logs|down|reset
./result/bin/caos-cli run|...
```
