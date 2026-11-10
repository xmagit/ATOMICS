# The host and agents share a UID; secret values therefore belong to a separate service user.
{ config, lib, pkgs, ... }:
let
  vm = config.atomics.vm;
  cfg = vm.secretBroker;
  user = "atomics-secret-broker";
  group = "atomics-secret-admin";
  useGroup = "atomics-secret-use";
  state = "/var/lib/atomics-secret-broker";
  runtime = "/run/atomics-secret-broker";
in {
  options.atomics.vm.secretBroker.enable = lib.mkEnableOption ''
    the headless secret broker, with a private store and a separate administration socket.
    Pair with atomics.vm.hardened so agents cannot become root and read its store
  '';

  config = lib.mkIf cfg.enable {
    assertions = [ {
      assertion = vm.adminUser != vm.user && vm.user != user && vm.adminUser != user;
      message = "The broker, VM and administration users must differ";
    } ];
    warnings = lib.optional (!vm.hardened)
      "atomics.vm.secretBroker without atomics.vm.hardened: agents can become root and read the store.";
    users.groups.${group} = { };
    users.groups.${useGroup} = { };
    users.users.${vm.user}.extraGroups = [ useGroup ];
    users.users.${user} = {
      isSystemUser = true;
      group = group;
      home = state;
      shell = pkgs.shadow + "/bin/nologin";
    };
    users.users.${vm.adminUser} = {
      isNormalUser = true;
      uid = 1001;
      extraGroups = [ group useGroup ];
    };
    # The broker owns the socket parent; agents cannot unlink or replace either socket.
    systemd.tmpfiles.rules = [ "d ${runtime} 0755 ${user} ${group} - -" ];
    environment.sessionVariables.ATOMICS_SECRET_BROKER_SOCKET = "${runtime}/use.sock";
    systemd.services.atomics-secret-broker = {
      description = "ATOMICS private secret broker";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      serviceConfig = {
        User = user;
        Group = group;
        SupplementaryGroups = [ useGroup ];
        StateDirectory = "atomics-secret-broker";
        StateDirectoryMode = "0700";
        UMask = "0077";
        LimitCORE = 0;
        ExecStart = "${config.programs.atomics.package}/bin/atomics secret-broker --host-uid ${toString config.users.users.${vm.user}.uid} --admin-uid ${toString config.users.users.${vm.adminUser}.uid} --use-group ${useGroup}";
        Restart = "on-failure";
        RestartSec = 2;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" ];
        CapabilityBoundingSet = "";
        ReadWritePaths = [ runtime ];
      };
    };
    systemd.services.atomics-host = lib.mkIf vm.host.enable {
      requires = [ "atomics-secret-broker.service" ];
      after = [ "atomics-secret-broker.service" ];
      environment.ATOMICS_SECRET_BROKER_SOCKET = "${runtime}/use.sock";
      # Change the host unit too, so a live upgrade gives its process the new access group.
      serviceConfig.SupplementaryGroups = [ useGroup ];
    };
  };
}
