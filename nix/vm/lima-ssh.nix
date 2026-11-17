# Lima refreshes cidata on every start. NixOS owns both the accounts and the SSH host keys.
{ config, lib, pkgs, ... }:
let
  cfg = config.atomics.vm;
  cidata = "/mnt/lima-cidata";
  logins = [ cfg.user ] ++ lib.optional cfg.hardened cfg.adminUser;
  provision = pkgs.writeShellScript "atomics-ssh-keys" ''
    set -euo pipefail
    export PATH=${lib.makeBinPath [ pkgs.coreutils pkgs.util-linux pkgs.yq-go pkgs.jq config.services.openssh.package ]}
    # Older images let cloud-init grant sudo to the Lima account.
    rm -f -- /etc/sudoers.d/90-cloud-init-users
    findmnt -rn -M ${cidata} -O ro >/dev/null
    exec ${pkgs.bash}/bin/bash ${./lima-ssh-keys.sh} ${cidata}/user-data /etc/ssh/authorized_keys.d ${lib.escapeShellArgs logins}
  '';
in {
  assertions = map (login: {
    assertion = let keys = config.users.users.${login}.openssh.authorizedKeys;
      in keys.keys == [ ] && keys.keyFiles == [ ];
    message = "Lima manages /etc/ssh/authorized_keys.d/${login}. Remove users.users.${login}.openssh.authorizedKeys for this account.";
  }) logins;

  services.cloud-init = {
    enable = true;
    settings = {
      datasource_list = [ "NoCloud" ];
      users = [ cfg.user ];
      disable_root = true;
      preserve_hostname = true;
      cloud_init_modules = lib.mkForce [ "seed_random" ];
      cloud_config_modules = lib.mkForce [ ];
      cloud_final_modules = lib.mkForce [ ];
    };
  };
  services.openssh = {
    enable = true;
    authorizedKeysInHomedir = false;
    settings = {
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
      PermitRootLogin = "no";
    };
  };

  # vm update stages the next boot. Keep the old daemon working until shutdown, and
  # provision before the new daemon accepts its first connection.
  systemd.services.atomics-ssh-keys = {
    description = "Install Lima's SSH login keys";
    requiredBy = [ "sshd.service" ];
    before = [ "sshd.service" ];
    unitConfig.RequiresMountsFor = cidata;
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ProtectHome = true;
      ExecStart = provision;
    };
  };

  # A live generation switch can replace /etc/ssh/sshd_config while sshd is still
  # serving. Install the keys before activation publishes that configuration.
  system.activationScripts.atomicsSshKeys.text = ''
    if [ -e /run/current-system ]; then
      ${provision} || exit 1
    fi
  '';
  system.activationScripts.etc.deps = [ "atomicsSshKeys" ];
}
