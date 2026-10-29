# The watchdog-prod host: a machine whose job is to notice caosd-prod failing
# and to do something about it. It runs no docker and no caos stack, on
# purpose: the thing watching the server must not share its failure modes.
#
# Apply it with
#
#   sudo nixos-rebuild switch --refresh --flake github:Metta-AI/caos#watchdog-prod
#
# A bare rebuild works for this role, unlike caosd-prod: nothing here is
# per-machine, because the one fact that is (which instance to watch) is a
# property of the fleet, not of this box, and is set in flake.nix.
#
# Three things run:
#
#  - the Datadog agent (prod/base.nix), so this host is itself watched;
#  - `caos-watchdog probe` from a timer every two minutes, reporting two
#    service checks to Datadog: can a client reach the server over iroh, can
#    an operator reach the box over SSM;
#  - `caos-watchdog respond`, the daemon that polls Datadog for alerting caos
#    monitors and runs a Claude Code session over them. prod/watchdog/
#    respond.go has the rules; runbook/CLAUDE.md is what the session reads.
#
# The responder's reach is three secrets and one IAM role. The role (metta:
# devops/tf/caos) may run SSM commands on the one prod instance and read the
# four secrets; it may not reboot, stop or launch anything. Reboots are a
# human's, asked for in Slack.

{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.caos.watchdog;
  stateDir = "/var/lib/caos-watchdog";
  # What the binary reads (main.go conf()); the same for the daemon, the
  # probe and an operator's login shell, so `caos-watchdog ssh` from a shell
  # talks to the same host and the same ledger.
  watchdogEnv = {
    CAOS_WATCHDOG_STATE = stateDir;
    CAOS_WATCHDOG_TARGET = cfg.targetInstance;
    CAOS_WATCHDOG_RUNBOOK = "${runbook}";
    CAOS_WATCHDOG_MODEL = cfg.model;
  };
  runbook = pkgs.runCommand "caos-watchdog-runbook" { } ''
    mkdir -p $out
    cp ${./runbook/CLAUDE.md} $out/CLAUDE.md
    cp ${./runbook/settings.json} $out/settings.json
  '';
in
{
  imports = [ ../base.nix ];

  options.caos.watchdog = {
    package = lib.mkOption {
      type = lib.types.package;
      description = "The caos-watchdog build. Supplied by the flake so the host runs THIS revision.";
    };
    clientTools = lib.mkOption {
      type = lib.types.package;
      description = ''
        caos-tools, for git-remote-caos and caos-iroh: the probe's ls-remote
        is a real client call, through the same binaries a user runs.
      '';
    };
    targetInstance = lib.mkOption {
      type = lib.types.str;
      description = "The EC2 instance id of the caosd-prod host this box watches.";
      example = "i-0c0aae25be3e44d8e";
    };
    model = lib.mkOption {
      type = lib.types.str;
      default = "claude-opus-5-5";
      description = "The model the responder session runs on.";
    };
  };

  config = {
    # Claude Code ships as a binary under Anthropic's terms; nixpkgs marks it
    # unfree. Named, not blanket: nothing else here needs the exception.
    nixpkgs.config.allowUnfreePredicate = pkg: builtins.elem (lib.getName pkg) [ "claude-code" ];

    caos.datadog.units = [
      "caos-watchdog.service"
      "caos-watchdog-probe.timer"
      "datadog-agent.service"
      "amazon-ssm-agent.service"
    ];

    caos.secrets = {
      datadog-app-key = {
        secretId = "caos/watchdog/datadog-app-key";
        group = "caos-watchdog";
      };
      anthropic-api-key = {
        secretId = "caos/watchdog/anthropic-api-key";
        group = "caos-watchdog";
      };
      slack-webhook = {
        secretId = "caos/watchdog/slack-webhook";
        group = "caos-watchdog";
      };
    };

    users.groups.caos-watchdog = { };
    users.users.caos-watchdog = {
      isSystemUser = true;
      group = "caos-watchdog";
      home = stateDir;
      createHome = true;
    };
    # The operator arrives as ssm-user; let them run `caos-watchdog ssh` and
    # read the ledger without sudo.
    users.users.ssm-user.extraGroups = [ "caos-watchdog" ];

    systemd.tmpfiles.rules = [
      "d ${stateDir} 0750 caos-watchdog caos-watchdog -"
    ];

    environment.variables = watchdogEnv;

    systemd.services.caos-watchdog = {
      description = "caos watchdog: respond to alerting Datadog monitors";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [
        "network-online.target"
        "caos-secrets.service"
      ];
      requires = [ "caos-secrets.service" ];
      path = [
        cfg.package
        cfg.clientTools
        pkgs.claude-code
        pkgs.awscli2
        pkgs.git
        pkgs.coreutils
      ];
      environment = watchdogEnv // {
        HOME = stateDir;
      };
      serviceConfig = {
        User = "caos-watchdog";
        Group = "caos-watchdog";
        WorkingDirectory = stateDir;
        ExecStart = "${cfg.package}/bin/caos-watchdog respond";
        # Any failure ends the process loudly (caos/w); systemd brings it back
        # and the ledger on disk means nothing is lost. No start limit: a
        # Datadog outage must not leave the watchdog permanently stopped.
        Restart = "always";
        RestartSec = 15;
      };
      unitConfig.StartLimitIntervalSec = 0;
    };

    systemd.services.caos-watchdog-probe = {
      description = "caos watchdog: probe the prod host and report to Datadog";
      after = [
        "caos-secrets.service"
        "datadog-agent.service"
      ];
      path = [
        cfg.package
        cfg.clientTools
        pkgs.awscli2
        pkgs.git
        pkgs.coreutils
      ];
      environment = watchdogEnv // {
        HOME = stateDir;
      };
      serviceConfig = {
        Type = "oneshot";
        User = "caos-watchdog";
        Group = "caos-watchdog";
        WorkingDirectory = stateDir;
        ExecStart = "${cfg.package}/bin/caos-watchdog probe";
        TimeoutStartSec = "5min";
      };
    };
    systemd.timers.caos-watchdog-probe = {
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnBootSec = "2min";
        OnUnitActiveSec = "2min";
        Unit = "caos-watchdog-probe.service";
      };
    };

    environment.systemPackages = [
      cfg.package
      cfg.clientTools
      pkgs.claude-code
    ];
  };
}
