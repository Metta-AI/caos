# What every caos host in EC2 shares: the parts of caosd/configuration.nix
# that a second host turned out to want verbatim. Roles import this and add
# their own daemon.
#
# Two things live here on purpose and one does not:
#
#  - `caos.secrets`: a role names the Secrets Manager entries it needs and
#    gets each as a file under /run/caos-secrets before its units start. The
#    instance role is the credential, so a secret never sits in the nix store
#    or in user-data.
#  - The Datadog agent, keyed from one of those secrets, with the systemd
#    integration watching whatever units the role names. Every caos host
#    reports to Datadog; the watchdog reads Datadog's monitor state back.
#  - NOT docker. The daemon host runs containers, the watchdog must not: a
#    box whose job is to notice the other one failing should not share its
#    failure modes (the veth/IPv4LL route below is one of them).

{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.caos;
  secretsDir = "/run/caos-secrets";
in
{
  options.caos = {
    secrets = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.submodule {
          options = {
            secretId = lib.mkOption {
              type = lib.types.str;
              description = "The Secrets Manager SecretId (name or ARN) to fetch.";
            };
            group = lib.mkOption {
              type = lib.types.str;
              default = "root";
              description = "Group that may read the file; the file is 0440 root:<group>.";
            };
          };
        }
      );
      default = { };
      description = ''
        Secrets fetched from AWS Secrets Manager at boot into
        ${secretsDir}/<name>, using the instance role. A unit that reads one
        orders itself `after = [ "caos-secrets.service" ]` (the Datadog agent
        below does). Absent credentials or a missing secret fail the unit
        loudly rather than start the consumer with an empty file.
      '';
    };

    datadog.units = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "amazon-ssm-agent.service" ];
      description = ''
        systemd units the Datadog systemd integration watches on this host.
        Each becomes a `systemd.unit.state` service check tagged `unit:<name>`;
        the caos monitors alert on any of them leaving `active`.
      '';
    };

    datadog.tags = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Extra host-level Datadog tags beyond service:caos and env:production.";
    };
  };

  config = {
    system.stateVersion = "25.11";

    # A flake-managed machine must NOT also be user-data-managed. The NixOS EC2
    # image runs amazon-init on EVERY boot, which copies user-data over
    # /etc/nixos/configuration.nix and switches to it -- silently reverting any
    # `nixos-rebuild --flake` on the next reboot. The repo is the source of
    # truth now, so this is off.
    virtualisation.amazon-init.enable = false;

    # Shell without opening a single inbound TCP port.
    services.amazon-ssm-agent.enable = true;

    nix.settings = {
      experimental-features = [
        "nix-command"
        "flakes"
      ];
      trusted-users = [
        "root"
        "@wheel"
      ];
      # GC when the root gets tight rather than dying with ENOSPC mid-build.
      min-free = 10 * 1024 * 1024 * 1024;
      max-free = 50 * 1024 * 1024 * 1024;

      # How long nix may reuse its answer to "what commit is this branch at".
      # The default is an hour, and a deploy names a BRANCH, so within that
      # hour a push is invisible here and the deploy silently rebuilds the
      # revision it already has. Observed: a box stayed pinned to 1cff83e
      # while the branch had moved on, until --refresh was passed. These are
      # deploy targets told to fetch, not build farms keeping an index warm.
      tarball-ttl = 5;

      # A private binary cache in the account, so a host never has to BUILD
      # caos: the Rust workspace is hours on a t3.small and minutes even on
      # the server. A deploy builds on a workstation (or the server) and
      # pushes; the host substitutes. Reads go through the instance role
      # (s3:GetObject on the bucket); pushes need the signing key, kept in
      # Secrets Manager as caos/nix-cache-signing-key. See prod/README.md.
      substituters = [
        "s3://caos-nix-cache-109251245574?region=us-east-1"
        "https://cache.nixos.org"
      ];
      trusted-public-keys = [
        "caos-cache-1:3BSj35RzBLAtUyTlJlLO3xa9b7uYFtIJIx17Jri8lPs="
        "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
      ];
    };

    # Docker gives every container a host-side veth. dhcpcd treats it as an
    # ordinary NIC, fails DHCP, and falls back to RFC3927 link-local --
    # installing 169.254.0.0/16 on that veth, which swallows 169.254.169.254,
    # the EC2 metadata service. SSM workers authenticate against IMDS at
    # startup, so the box loses its only entrance the moment a container
    # starts, while EC2 still reports both status checks ok. Observed as:
    #   169.254.0.0/16 dev vethf53fd81 scope link src 169.254.23.62
    # Two independent guards, because losing this costs the whole machine.
    # Harmless on a host without docker, and kept in the base so a role that
    # grows a container later does not rediscover it.
    networking.dhcpcd = {
      denyInterfaces = [
        "veth*"
        "docker*"
        "br-*"
        "caos*"
      ];
      extraConfig = "noipv4ll";
    };

    # The secrets fetch. One oneshot for all of them, so a consumer orders
    # after a single unit. Fails if any secret is missing: a half-populated
    # directory would start the agent keyless and the responder mute, each
    # of which looks like "quiet" from the outside.
    systemd.services.caos-secrets = lib.mkIf (cfg.secrets != { }) {
      description = "fetch caos secrets from AWS Secrets Manager";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      path = [
        pkgs.awscli2
        pkgs.coreutils
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        RuntimeDirectory = "caos-secrets";
        RuntimeDirectoryMode = "0755";
        RuntimeDirectoryPreserve = true;
      };
      script = lib.concatStringsSep "\n" (
        lib.mapAttrsToList (name: s: ''
          aws secretsmanager get-secret-value --region us-east-1 \
            --secret-id ${lib.escapeShellArg s.secretId} \
            --query SecretString --output text > ${secretsDir}/${name}.tmp
          # Secrets Manager returns the value verbatim and `--output text`
          # appends a newline; strip it so a file-reading consumer gets the
          # key, not key-plus-newline.
          printf '%s' "$(cat ${secretsDir}/${name}.tmp)" > ${secretsDir}/${name}
          rm ${secretsDir}/${name}.tmp
          chown root:${s.group} ${secretsDir}/${name}
          chmod 0440 ${secretsDir}/${name}
        '') cfg.secrets
      );
    };

    caos.secrets.datadog-api-key = {
      secretId = "caos/datadog-api-key";
      group = "datadog";
    };

    services.datadog-agent = {
      enable = true;
      apiKeyFile = "${secretsDir}/datadog-api-key";
      site = "datadoghq.com";
      hostname = config.networking.hostName;
      tags = [
        "service:caos"
        "env:production"
        "role:${config.networking.hostName}"
      ] ++ cfg.datadog.tags;
      # Tag disk metrics by mount point (device:/data) rather than by the
      # kernel's device name, which EBS does not keep stable across reboots.
      diskCheck = {
        init_config = { };
        instances = [ { use_mount = true; } ];
      };
      checks.systemd = {
        init_config = { };
        instances = [ { unit_names = cfg.datadog.units; } ];
      };
    };
    users.users.datadog.extraGroups = [ "systemd-journal" ];
    systemd.services.datadog-agent = {
      after = [ "caos-secrets.service" ];
      requires = [ "caos-secrets.service" ];
    };

    environment.systemPackages = [
      pkgs.awscli2
      pkgs.git
      pkgs.tmux
      pkgs.htop
      pkgs.jq
    ];
  };
}
