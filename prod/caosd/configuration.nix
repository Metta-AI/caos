# The caosd-prod host: a machine whose job is to run `caosd up --iroh`.
#
# Apply it with the deploy driver, NOT with nixos-rebuild:
#
#   nix run github:Metta-AI/caos#deploy-caosd-prod
#
# `nixos-rebuild switch --flake ...#caosd-prod` cannot work on its own: this
# role leaves caos.advertiseAddress undefined because it differs per machine,
# so a bare rebuild fails on that option. prod/caosd/deploy reads it off the
# host and layers it on with extendModules -- see prod/README.md.
#
# Nothing here is specific to one instance: the address is supplied at deploy
# time and the data disk is found by label, so a fresh machine needs no edit.

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

    relay.enable = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Run an iroh relay here. caos uses no other: n0's are never dialled,
        for a relay or for discovery, so SOMEONE has to run one and every
        `caos://` endpoint is required to name it.

        A Claude Code cloud container's SETUP phase is one such network: all
        seven relays iroh ships answer 503 there with an Envoy upstream connect
        error -- under h2 and http/1.1 alike, and by bare IP -- while
        www.hetzner.com answers 200 from the same phase and four of those relays
        are Hetzner-hosted. It refuses those destinations, not relay traffic: a
        relay run HERE answered 200 from that phase, on the host and port where
        an ordinary HTTP server had.

        A dev stack points at it with CAOS_IROH_RELAY, which `caosd up --iroh`
        requires and passes to `caos-iroh serve --relay`. There is no fallback:
        one that existed minted a ticket carrying the same endpoint id and
        token as the one already in someone's environment, so it looked current
        while reaching nothing.
      '';
    };

    relay.port = lib.mkOption {
      type = lib.types.port;
      default = 80;
      description = ''
        Port for the relay. 80 or 443 and nothing else: that egress carries no
        other port, measured -- the same relay on 3340 timed out from both the
        setup and the session phase, while port 80 answered from both.
      '';
    };

    relay.url = lib.mkOption {
      type = lib.types.str;
      default =
        if cfg.relay.enable then
          "http://${cfg.advertiseAddress}${
            lib.optionalString (cfg.relay.port != 80) ":${toString cfg.relay.port}"
          }/"
        else
          "";
      description = ''
        The relay this stack's endpoint is reached through, baked into its
        ticket. Required: caos dials no relay it was not given, so `caosd up
        --iroh` refuses to start without one.

        THE PUBLIC ADDRESS, not localhost, because this one string does two
        jobs: it is what this endpoint connects to AND what every client dials
        out of the ticket. A loopback URL would leave the ticket naming a relay
        only this machine can reach.

        It defaults to the relay this host runs (relay.enable). A machine that
        runs none must name someone else's, and the assertion below says so
        rather than letting the service fail at start.
      '';
      example = "http://34.200.32.255/";
    };
  };

  config = {
    system.stateVersion = "25.11";

    # Refused at BUILD time, where the option is named, rather than at start
    # where the failure is a dead service on a machine that was serving.
    assertions = [
      {
        assertion = cfg.relay.url != "";
        message = ''
          caos.relay.url is empty: this host runs no relay of its own
          (caos.relay.enable = false), so it must name one to be reached
          through. n0's relays are not used by caos at all.
        '';
      }
    ];

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
    networking.firewall.allowedTCPPorts = lib.optional cfg.relay.enable cfg.relay.port;

    # The relay, as a UNIT rather than a shell command, because `nix run` dies
    # with the terminal that started it -- and a dead relay is indistinguishable
    # from the gateway blocking us, a confusion that has already cost two
    # debugging rounds.
    #
    # `--dev` is plain HTTP, and that is deliberate rather than provisional: the
    # relay client reads TLS off the URL scheme, so an `http://` relay URL needs
    # no certificate and no DNS name, and the hop carries nothing readable --
    # payloads stay end-to-end encrypted between endpoint keys, so the relay
    # sees ciphertext and metadata either way. TLS here would buy privacy for
    # the metadata and a way to stop strangers relaying through this box; it is
    # not what makes the transport work.
    systemd.services.iroh-relay = lib.mkIf cfg.relay.enable {
      description = "iroh relay, for clients whose network cannot reach n0's";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      serviceConfig = {
        ExecStart = "${pkgs.iroh-relay}/bin/iroh-relay --dev --config-path ${
          pkgs.writeText "iroh-relay.toml" ''
            http_bind_addr = "[::]:${toString cfg.relay.port}"
            enable_metrics = false
          ''
        }";
        Restart = "always";
        RestartSec = 2;
        DynamicUser = true;
        # Port 80 is privileged and this does not run as root. Ambient rather
        # than a root ExecStart: the relay needs to bind low and nothing else.
        AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
        CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
      };
    };

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
        CAOS_IROH_RELAY = cfg.relay.url;
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
