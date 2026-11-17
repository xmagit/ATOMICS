# Exercise the image's key installer with a read-only stand-in for Lima's cidata.
{ pkgs, vm }:

let
  inherit (pkgs) lib;
  inherit (vm.atomics.vm) user hardened adminUser;
  logins = [ user ] ++ lib.optional hardened adminUser;
  inherit (import "${pkgs.path}/nixos/tests/ssh-keys.nix" pkgs) snakeOilPrivateKey snakeOilPublicKey;
  hostKeys = map (key: key.path) vm.services.openssh.hostKeys;
  fingerprints = "${pkgs.runtimeShell} ${./vm-ssh-host-key-fingerprints.sh} "
    + lib.escapeShellArgs (map (path: "${path}.pub") hostKeys);
in
assert vm.services.cloud-init.enable && vm.services.cloud-init.config == "";
assert vm.services.cloud-init.settings.cloud_init_modules == [ "seed_random" ];
assert vm.services.openssh.authorizedKeysFiles == [ "/etc/ssh/authorized_keys.d/%u" ];
assert vm.services.openssh.generateHostKeys && hostKeys != [ ];
pkgs.testers.runNixOSTest {
  name = if hardened then "atomics-vm-hardened-login-keys" else "atomics-vm-ssh-host-keys";
  nodes.machine = { ... }: {
    imports = [ ../vm/lima-ssh.nix ];
    options.atomics.vm = lib.mkOption { default = { inherit user hardened adminUser; }; };
    config = {
      system.stateVersion = vm.system.stateVersion;
      users.users = lib.genAttrs logins (_: { isNormalUser = true; });
      services.openssh = { inherit (vm.services.openssh) hostKeys generateHostKeys; };
      systemd.services.test-cidata = {
        requiredBy = [ "cloud-init-local.service" "atomics-ssh-keys.service" ];
        before = [ "cloud-init-local.service" "atomics-ssh-keys.service" ];
        path = [ pkgs.util-linux ];
        serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
        script = ''
          seed=/var/lib/cloud/seed/nocloud
          install -d -m 0700 "$seed" /mnt/lima-cidata
          instance_id=$(cat /proc/sys/kernel/random/boot_id)
          printf 'instance-id: %s\n' "$instance_id" > "$seed/meta-data"
          cat > "$seed/user-data" <<EOF_DATA
          #cloud-config
          users:
            - name: ${user}
              ssh-authorized-keys:
                - ${snakeOilPublicKey} boot-$instance_id
          EOF_DATA
          mount -o bind,ro "$seed" /mnt/lima-cidata
        '';
      };
      specialisation.updated.configuration.environment.etc."host-key-update".text = "updated\n";
    };
  };

  testScript = ''
    def ready():
        machine.wait_for_unit("cloud-init.service")
        machine.wait_for_unit("sshd.service")
        machine.wait_for_open_port(22)
        machine.succeed("test $(cat /var/lib/cloud/data/instance-id) = $(cat /proc/sys/kernel/random/boot_id)")
        machine.succeed("install -m 0600 ${snakeOilPrivateKey} /run/test-ssh-key")
        for user in ${builtins.toJSON logins}:
            key = f"/etc/ssh/authorized_keys.d/{user}"
            machine.succeed(f"test \"$(stat -c '%u:%g %a' {key})\" = '0:0 444'")
            machine.succeed(f"grep -F boot-$(cat /proc/sys/kernel/random/boot_id) {key}")
            login = machine.succeed(f"ssh -o LogLevel=ERROR -o BatchMode=yes -o StrictHostKeyChecking=accept-new -i /run/test-ssh-key {user}@localhost \"bash -lc 'id -un'\"")
            assert login.strip() == user, login

    machine.start(allow_reboot=True)
    ready()
    original = machine.succeed("${fingerprints}")
    assert original.count("SHA256:") == ${toString (builtins.length hostKeys)}, original
    with subtest("a home-directory key path is ignored across a reboot and login"):
        machine.succeed("install -d -m 0700 /var/lib/test-login-keys && echo original > /var/lib/test-login-keys/canary")
        machine.succeed("runuser -u ${user} -- sh -c 'mkdir -p ~/.ssh && ln -sf /var/lib/test-login-keys/canary ~/.ssh/authorized_keys'")
        machine.reboot()
        ready()
        machine.succeed("grep -Fx original /var/lib/test-login-keys/canary && test $(wc -l </var/lib/test-login-keys/canary) = 1")
        machine.succeed("test ! -e /etc/sudoers.d/90-cloud-init-users")
        assert machine.succeed("${fingerprints}") == original

    with subtest("host and login keys survive a live activation and another boot"):
        previous = machine.succeed("readlink -f /run/current-system").strip()
        machine.succeed(f"{previous}/specialisation/updated/bin/switch-to-configuration test")
        machine.succeed("test -f /etc/host-key-update")
        machine.succeed("systemctl restart sshd.service")
        ready()
        assert machine.succeed("${fingerprints}") == original
        machine.reboot()
        ready()
        assert machine.succeed("${fingerprints}") == original
  '';
}
