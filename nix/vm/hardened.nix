# Hardened mode (docs/nixos-vm.md, Hardened mode): the VM's user, and so the host, the chief and every
# agent, cannot become root, write the Mac's folders or run root's Docker. Without it that user is in
# wheel and docker, either as good as root, and nothing inside the VM holds against an agent: not a
# network policy, not the guest's own configuration.
{ config, lib, pkgs, ... }:

let
  inherit (lib) mkOption types;
  cfg = config.atomics.vm;
  desktop = config.programs.atomics.macDesktop;
  uid = toString config.users.users.${cfg.user}.uid;
  runtime = "/run/user/${uid}";
  dockerSocket = "${runtime}/docker.sock";
in
{
  options.atomics.vm = {
    hardened = lib.mkEnableOption ''
      hardened mode: the VM's user loses sudo and the docker group, gets rootless Docker instead, and
      mounts the Mac's folders read-only. Updates and repairs go through the admin user, whom only the
      Mac's Lima key reaches
    '';

    nixForAgents = mkOption {
      type = types.bool;
      default = true;
      description = ''
        In hardened mode, allow agents to use the Nix daemon for builds, nix develop and nix-shell.
        False restricts daemon access to root and the admin user, preventing agents from requesting
        daemon-owned network fetches that bypass the build users' egress rules.
      '';
    };

    adminUser = mkOption {
      type = types.strMatching "[a-z_][a-z0-9_-]*";
      default = "admin";
      description = ''
        In hardened mode, the one user with sudo. Only the key Lima gives the VM's user reaches it, read at
        every boot from Lima's read-only cidata, so nothing the VM's user can write changes who logs in as it.
        `atomics vm update` builds and switches the system as this user.
      '';
    };
  };

  config = lib.mkIf cfg.hardened {
    assertions = [ {
      assertion = cfg.adminUser != cfg.user;
      message = "atomics.vm.adminUser must differ from atomics.vm.user";
    } ];

    users.users.${cfg.adminUser} = {
      isNormalUser = true;
      uid = 1001;
      extraGroups = [ "wheel" ];
      description = "ATOMICS VM updates and repairs";
    };
    # Rootless Docker runs under the user's own systemd, from boot, in a user namespace of its own.
    users.users.${cfg.user} = {
      linger = true;
      autoSubUidGidRange = true;
    };

    # Build users are filtered by egress, but daemon-owned fetches still run as root. Keep normal
    # agent development working by default; teams can opt out of daemon access for a stricter boundary.
    nix.settings.allowed-users = lib.mkIf (!cfg.nixForAgents) (lib.mkForce [ "root" cfg.adminUser ]);

    # cloud-init writes Lima's user data on every boot, and that gives the VM's user passwordless sudo
    # (/etc/sudoers.d/90-cloud-init-users). This removes it again before the host starts, and gives the admin
    # the same Lima key, from the cidata disk the VM only reads.
    systemd.services.atomics-harden = {
      description = "Keep root away from the ATOMICS VM's user";
      wantedBy = [ "multi-user.target" ];
      after = [ "cloud-init.service" ];
      before = [ "lima-ready.service" ];
      unitConfig.RequiresMountsFor = "/mnt/lima-cidata";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        set -euo pipefail
        rm -f /etc/sudoers.d/90-cloud-init-users
        keys=$(${pkgs.yq-go}/bin/yq -r '.users[] | select(.name == "${cfg.user}") | ."ssh-authorized-keys"[]' \
          /mnt/lima-cidata/user-data)
        test -n "$keys"
        install -d -m 0755 /etc/ssh/authorized_keys.d
        printf '%s\n' "$keys" > /etc/ssh/authorized_keys.d/.${cfg.adminUser}.new
        chmod 0444 /etc/ssh/authorized_keys.d/.${cfg.adminUser}.new
        mv -f /etc/ssh/authorized_keys.d/.${cfg.adminUser}.new /etc/ssh/authorized_keys.d/${cfg.adminUser}
      '';
    };
    systemd.services.lima-ready = {
      requires = [ "atomics-harden.service" ];
      after = [ "atomics-harden.service" ];
    };

    # Belt and braces: nothing the host starts can gain privileges through a setuid program.
    systemd.services.atomics-host = lib.mkIf cfg.host.enable {
      requires = [ "atomics-harden.service" ];
      after = [ "atomics-harden.service" ];
      environment = {
        DOCKER_HOST = "unix://${dockerSocket}";
        XDG_RUNTIME_DIR = runtime;
      };
      serviceConfig.NoNewPrivileges = true;
    };

    # The user's own Docker: containers run in its user namespace, so they get no more than the user has,
    # and their traffic leaves as the user's, under the same network policy (atomics.vm.egress). Tools that
    # only know /var/run/docker.sock find it there.
    virtualisation.docker.rootless = {
      enable = true;
      setSocketVariable = true;
      # The host's resolver is loopback-only (dnsmasq), which Docker drops for containers and replaces with
      # Google's. slirp4netns answers on 10.0.2.3 and asks the host's resolver, under its policy.
      daemon.settings.dns = [ "10.0.2.3" ];
    };
    # Only the VM's user: an admin login would otherwise start a Docker of its own.
    systemd.user.services.docker.unitConfig.ConditionUser = lib.mkForce cfg.user;
    systemd.tmpfiles.rules = [ "L+ /run/docker.sock - - - - ${dockerSocket}" ];
    # A rootless container can raise no limit above its daemon's, and root's Docker had locked memory unlimited:
    # Elasticsearch, for one, asks for that. The user's systemd holds the ceiling, and its Docker takes it.
    systemd.services."user@".serviceConfig.LimitMEMLOCK = "infinity";
    systemd.user.services.docker.serviceConfig.LimitMEMLOCK = "infinity";

    # Only this Mac's ATOMICS updates the VM, as the admin; the bundle tells it so.
    programs.atomics.macDesktop.adminUser = cfg.adminUser;
    # Read-only unless a module says otherwise: with the host in the VM, agents work on the VM's disk, and a
    # writable share would let them plant git hooks or modules that run on the Mac.
    programs.atomics.macDesktop.shares = lib.mkDefault {
      "${desktop.home}/repos" = { };
      "${desktop.home}/.atomics/briefs" = { };
    };
  };
}
