# The caos host: a machine whose job is to run `caosd up --iroh`.
#
# Built to be reproducible from the repo alone:
#   sudo nixos-rebuild switch --flake github:Metta-AI/caos#caos-dev
#
# Nothing here is specific to one instance. The public address comes from IMDS
# at boot and the data disk is found by label, so a fresh machine needs no edit.

{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.caos;
in
{
  options.caos = {
    package = lib.mkOption {
      type = lib.types.package;
      description = "The caos-tools build providing bin/caosd. Supplied by the flake so the machine runs THIS revision.";
    };
    dataDevice = lib.mkOption {
      type = lib.types.str;
      default = "/dev/disk/by-label/caos-data";
      description = ''
        The bulk volume for docker images, the registry, redis and the server repo.
        By label, because EBS device enumeration order is not stable. Label it once:
          mkfs.ext4 -L caos-data /dev/nvme1n1
      '';
    };
    advertiseAddress = lib.mkOption {
      type = lib.types.str;
      description = ''
        The address clients reach this machine on, baked into the iroh ticket.

        No default: this is per-machine, so it cannot live in a role config
        that several boxes share. A pure flake cannot read it either, so
        prod/caosd/deploy.sh reads it off the host and layers it on with
        extendModules. Leaving it unset fails the build with a named option
        rather than booting a stack that advertises a private address.
      '';
      example = "34.200.32.255";
    };

    irohPort = lib.mkOption {
      type = lib.types.port;
      default = 11204;
      description = "Fixed UDP port for the iroh transport; it must be stable to be in a ticket.";
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
      experimental-features = [ "nix-command" "flakes" ];
      trusted-users = [ "root" "@wheel" ];
      # /nix is on the root disk while the bulk data is on cfg.dataDevice. GC
      # when the root gets tight rather than dying with ENOSPC mid-build.
      min-free = 10 * 1024 * 1024 * 1024;
      max-free = 50 * 1024 * 1024 * 1024;
    };

    # Docker gives every container a host-side veth. dhcpcd treats it as an
    # ordinary NIC, fails DHCP, and falls back to RFC3927 link-local --
    # installing 169.254.0.0/16 on that veth, which swallows 169.254.169.254,
    # the EC2 metadata service. SSM workers authenticate against IMDS at
    # startup, so the box loses its only entrance the moment caos starts a
    # container, while EC2 still reports both status checks ok. Observed as:
    #   169.254.0.0/16 dev vethf53fd81 scope link src 169.254.23.62
    # Two independent guards, because losing this costs the whole machine.
    networking.dhcpcd = {
      denyInterfaces = [ "veth*" "docker*" "br-*" "caos*" ];
      extraConfig = "noipv4ll";
    };

    virtualisation.docker = {
      enable = true;
      # The `docker` alias still resolves to 28.5.2, which nixpkgs marks
      # unmaintained; an unpinned config refuses to evaluate at all.
      package = pkgs.docker_29;
      daemon.settings = {
        data-root = "/data/docker";
        # Docker's built-in pool is 172.17.0.0/12 in /16s, spanning
        # 172.16-172.31 -- so its 16th network is 172.31.0.0/16, exactly the
        # CIDR of every EC2 *default* VPC. caos's runner pool can allocate that
        # many. Move the pool somewhere nothing routes, and exhaustion then
        # fails a network create loudly instead of eating the VPC route.
        default-address-pools = [
          {
            base = "10.200.0.0/14";
            size = 24;
          }
        ];
      };
    };

    fileSystems."/data" = {
      device = cfg.dataDevice;
      fsType = "ext4";
      autoResize = true;
      # nofail: a missing data disk should degrade to a bootable machine with a
      # shell, not a brick nobody can log in to fix.
      options = [
        "nofail"
        "x-systemd.device-timeout=60"
      ];
    };

    # data-root lives under /data, so dockerd must not start first and build its
    # tree on the root disk only for the mount to shadow it.
    systemd.services.docker = {
      after = [ "data.mount" ];
      requires = [ "data.mount" ];
    };

    systemd.tmpfiles.rules = [
      "d /data      0755 root root -"
      "d /data/caos 0755 caos caos -"
    ];

    users.groups.caos = { };
    users.users.caos = {
      isSystemUser = true;
      group = "caos";
      home = "/data/caos";
      extraGroups = [ "docker" ];
    };
    # You arrive over SSM as ssm-user; let it drive the stack too.
    users.users.ssm-user.extraGroups = [ "docker" "caos" ];

    networking.firewall.allowedUDPPorts = [ cfg.irohPort ];

    # Answering "does it start on boot": yes, this is that.
    systemd.services.caosd = {
      description = "caos stack: redis, registry, server, runnerd, iroh";
      wantedBy = [ "multi-user.target" ];
      after = [
        "docker.service"
        "data.mount"
      ];
      requires = [
        "docker.service"
        "data.mount"
      ];
      # caosd shells out to these. systemd units get a minimal PATH, and the
      # failure is a bare `docker: command not found` at startup.
      path = [
        config.virtualisation.docker.package
        pkgs.git
        pkgs.gnutar
        pkgs.gzip
        pkgs.coreutils
      ];
      environment = {
        CAOS_DATA = "/data/caos";
        CAOS_IROH_ADVERTISE = "${cfg.advertiseAddress}:${toString cfg.irohPort}";
      };
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        User = "caos";
        Group = "caos";
        WorkingDirectory = "/data/caos";
        ExecStart = "${cfg.package}/bin/caosd up --iroh";
        ExecStop = "${cfg.package}/bin/caosd down";
        # A cold first run builds every std worker image before it returns.
        TimeoutStartSec = "90min";
      };
    };

    environment.systemPackages = [
      cfg.package
      pkgs.git
      pkgs.tmux
      pkgs.htop
      pkgs.jq
    ];
  };
}
