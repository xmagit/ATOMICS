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
  agentUser = config.users.users.${cfg.user};
  # Whether a wrapper's chmod clauses (applied after chmod 0000) let others, or a group the VM's user is in, run it.
  runnableBy = who: permissions: lib.any (clause:
    let
      parts = builtins.match "([ugoa]*)([-+=])([rwxXst]*)" clause;
      subjects = if parts == null then "a" else lib.elemAt parts 0;
    in
    parts == null || (lib.elemAt parts 1 != "-" && builtins.match ".*[xX].*" (lib.elemAt parts 2) != null
      && (subjects == "" || lib.hasInfix "a" subjects || lib.hasInfix who subjects))
  ) (lib.splitString "," permissions);
  agentRunnable = lib.attrNames (lib.filterAttrs (name: wrapper:
    wrapper.enable && !lib.elem name cfg.agentWrappers && (runnableBy "o" wrapper.permissions
      || (lib.elem wrapper.group (agentUser.extraGroups ++ [ agentUser.group ]) && runnableBy "g" wrapper.permissions))
  ) config.security.wrappers);
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

    agentWrappers = mkOption {
      type = types.listOf types.str;
      default = [ "newuidmap" "newgidmap" ];
      description = ''
        In hardened mode, the only setuid or capability wrappers in /run/wrappers/bin the VM's user may run.
        Rootless Docker needs the UID/GID mapping helpers. Every other wrapper must be restricted to the
        admin (group wheel, no permission for others), or the build fails and names it.
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
    } {
      assertion = agentRunnable == [ ];
      message = ''
        hardened mode: the VM's user could run these setuid or capability wrappers: ${lib.concatStringsSep ", " agentRunnable}.
        Restrict each to the admin (security.wrappers.<name> group "wheel", permissions "u+rx,g+rx,o-rwx"), or list it
        in atomics.vm.agentWrappers if agents must run it.
      '';
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

    # The user's systemd manager can start processes outside atomics-host's NoNewPrivileges.
    # Restrict execution at the wrappers themselves, including sudoedit via execWheelOnly.
    security.sudo.execWheelOnly = true;
    security.wrappers = lib.genAttrs [
      "su" "sg" "newgrp" "passwd" "chsh" "mount" "umount" "unix_chkpwd"
    ] (_: {
      owner = lib.mkForce "root";
      group = lib.mkForce "wheel";
      permissions = lib.mkForce "u+rx,g+rx,o-rwx";
    });
    # unix_chkpwd is unnecessary for the agents' key-only logins; keep password checks with the
    # admin too. Leave newuidmap/newgidmap available: rootless Docker needs their setuid mappings,
    # so NoNewPrivileges must not be imposed on the user's systemd manager. A wrapper added later,
    # by NixOS or a module, fails the build until it is restricted too (agentWrappers).

    # Build users are filtered by egress, but daemon-owned fetches still run as root. Keep normal
    # agent development working by default; teams can opt out of daemon access for a stricter boundary.
    nix.settings.allowed-users = lib.mkIf (!cfg.nixForAgents) (lib.mkForce [ "root" cfg.adminUser ]);

    # Nothing from the Mac runs here as a program: shares mount noexec too (and stay nosuid, nodev).
    programs.atomics.macDesktop.noexecShares = true;

    # Belt and braces: nothing the host starts can gain privileges through a setuid program.
    systemd.services.atomics-host = lib.mkIf cfg.host.enable {
      requires = [ "atomics-ssh-keys.service" ];
      after = [ "atomics-ssh-keys.service" ];
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
    # writable ~/repos would let them plant git hooks or modules that run on the Mac. The one writable share is
    # the exchange, for files in either direction, which ATOMICS on the Mac treats as untrusted data
    # (docs/nixos-vm.md, The exchange folder).
    programs.atomics.macDesktop.shares = lib.mkDefault {
      "${desktop.home}/repos" = { };
      "${desktop.home}/.atomics/briefs" = { };
      "${desktop.home}/atomics-exchange".writable = true;
    };
  };
}
