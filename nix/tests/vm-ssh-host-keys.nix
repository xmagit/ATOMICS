# Exercise the image's key installer with a read-only stand-in for Lima's cidata.
{ pkgs, vm }:

let
  inherit (pkgs) lib;
  inherit (vm.atomics.vm) user hardened adminUser;
  logins = [ user ] ++ lib.optional hardened adminUser;
  inherit (import "${pkgs.path}/nixos/tests/ssh-keys.nix" pkgs)
    snakeOilPrivateKey snakeOilPublicKey snakeOilEd25519PrivateKey snakeOilEd25519PublicKey;
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
  nodes.machine = { lib, ... }: {
    imports = [ ../vm/lima-ssh.nix ];
    options.atomics.vm = lib.mkOption { default = { inherit user hardened adminUser; }; };
    config = {
      system.stateVersion = vm.system.stateVersion;
      users.users = lib.genAttrs logins (_: { isNormalUser = true; });
      services.openssh = { inherit (vm.services.openssh) hostKeys generateHostKeys; };
      systemd.services.atomics-user-setup = let unit = vm.systemd.services.atomics-user-setup; in {
        wantedBy = [ "multi-user.target" ];
        inherit (unit) script;
        environment = { inherit (unit.environment) ATOMICS_CONFIG_SEED; };
        serviceConfig = { inherit (unit.serviceConfig) User Type RemainAfterExit UMask; };
      };
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
          key=${lib.escapeShellArg snakeOilPublicKey}
          if [ -f /var/lib/test-login-keys/rotate ]; then key=${lib.escapeShellArg snakeOilEd25519PublicKey}; fi
          if [ -f /var/lib/test-login-keys/invalid ]; then key=invalid; fi
          cat > "$seed/user-data" <<EOF_DATA
          #cloud-config
          users:
            - name: ${user}
              ssh-authorized-keys:
                - $key boot-$instance_id
          EOF_DATA
          mount -o bind,ro "$seed" /mnt/lima-cidata
        '';
      };
      specialisation.updated.configuration = {
        environment.etc."host-key-update".text = "updated\n";
        systemd.services.atomics-user-setup.environment.ATOMICS_CONFIG_SEED =
          lib.mkForce (pkgs.writeText "updated-seed.toml" "base_ref = \"updated\"\n");
      };
    };
  };

  testScript = ''
    first = "${snakeOilPrivateKey}"
    second = "${snakeOilEd25519PrivateKey}"

    def ready(current: str, obsolete: str):
        machine.wait_for_unit("cloud-init.service")
        machine.wait_for_unit("sshd.service")
        machine.wait_for_open_port(22)
        machine.succeed("test $(cat /var/lib/cloud/data/instance-id) = $(cat /proc/sys/kernel/random/boot_id)")
        machine.succeed("sshd -T | grep -Fix 'authorizedkeysfile /etc/ssh/authorized_keys.d/%u'")
        machine.succeed(f"install -m 0600 {current} /run/test-ssh-key && install -m 0600 {obsolete} /run/obsolete-key")
        ssh = "ssh -o LogLevel=ERROR -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o IdentitiesOnly=yes -o IdentityAgent=none"
        for user in ${builtins.toJSON logins}:
            key = f"/etc/ssh/authorized_keys.d/{user}"
            machine.succeed(f"test \"$(stat -c '%u:%g %a' {key})\" = '0:0 444'")
            machine.succeed(f"grep -F boot-$(cat /proc/sys/kernel/random/boot_id) {key}")
            login = machine.succeed(f"{ssh} -i /run/test-ssh-key {user}@localhost \"bash -lc 'id -un'\"")
            assert login.strip() == user, login
            machine.fail(f"{ssh} -i /run/obsolete-key {user}@localhost true")

    def home_snapshot():
        machine.wait_for_unit("atomics-user-setup.service")
        return machine.succeed("runuser -u ${user} -- sh -ec 'test -f ~/repos/kept && stat -c %a ~/.config ~/.config/atomics ~/.config/atomics/config.toml && sha256sum ~/.config/atomics/config.toml'")

    machine.start(allow_reboot=True)
    ready(first, second)
    machine.succeed("runuser -u ${user} -- test ! -e /home/${user}/.ssh/authorized_keys")
    machine.wait_for_unit("atomics-user-setup.service")
    machine.succeed("runuser -u ${user} -- sh -ec 'chmod 0711 ~/.config && echo \"# my settings\" >> ~/.config/atomics/config.toml && touch ~/repos/kept'")
    home = home_snapshot()
    assert home.splitlines()[:3] == ["711", "700", "600"], home
    original = machine.succeed("${fingerprints}")
    assert original.count("SHA256:") == ${toString (builtins.length hostKeys)}, original
    with subtest("a home-directory key path is ignored across a reboot and login"):
        machine.succeed("install -d -m 0700 /var/lib/test-login-keys && echo original > /var/lib/test-login-keys/canary")
        machine.succeed("touch /var/lib/test-login-keys/rotate")
        machine.succeed("runuser -u ${user} -- sh -c 'mkdir -p ~/.ssh && ln -sf /var/lib/test-login-keys/canary ~/.ssh/authorized_keys'")
        machine.reboot()
        ready(second, first)
        machine.succeed("grep -Fx original /var/lib/test-login-keys/canary && test $(wc -l </var/lib/test-login-keys/canary) = 1")
        machine.succeed("runuser -u ${user} -- test -L /home/${user}/.ssh/authorized_keys")
        machine.succeed("test ! -e /etc/sudoers.d/90-cloud-init-users")
        assert machine.succeed("${fingerprints}") == original
        assert home_snapshot() == home

    with subtest("host and login keys survive a live activation and another boot"):
        previous = machine.succeed("readlink -f /run/current-system").strip()
        machine.succeed(f"{previous}/specialisation/updated/bin/switch-to-configuration test")
        machine.succeed("test -f /etc/host-key-update")
        machine.succeed("systemctl restart sshd.service")
        ready(second, first)
        assert machine.succeed("${fingerprints}") == original
        assert home_snapshot() == home
        machine.reboot()
        ready(second, first)
        assert machine.succeed("${fingerprints}") == original
        assert home_snapshot() == home

    with subtest("malformed cidata prevents sshd from starting and preserves previously installed keys"):
        machine.succeed("sha256sum ${lib.concatMapStringsSep " " (name: "/etc/ssh/authorized_keys.d/${name}") logins} > /var/lib/test-login-keys/sums")
        machine.succeed("touch /var/lib/test-login-keys/invalid")
        machine.succeed("mkdir -p /etc/sudoers.d && echo '# obsolete' > /etc/sudoers.d/90-cloud-init-users")
        machine.reboot()
        machine.wait_until_succeeds("systemctl is-failed atomics-ssh-keys.service")
        machine.fail("systemctl is-active sshd.service")
        machine.succeed("sha256sum -c /var/lib/test-login-keys/sums && test ! -e /etc/sudoers.d/90-cloud-init-users")
  '';
}
