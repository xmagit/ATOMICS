# Use the image's cloud-init and sshd settings with real NoCloud provisioning. A new instance ID
# on every boot exercises the ssh module again, as Lima does when it refreshes cidata.
{ pkgs, vm }:

let
  inherit (pkgs) lib;
  user = vm.atomics.vm.user;
  inherit (import "${pkgs.path}/nixos/tests/ssh-keys.nix" pkgs) snakeOilPublicKey;
  hostKeys = map (key: key.path) vm.services.openssh.hostKeys;
  fingerprints = "${pkgs.runtimeShell} ${./vm-ssh-host-key-fingerprints.sh} "
    + lib.escapeShellArgs (map (path: "${path}.pub") hostKeys);
in
# These also run when only the driver can be built, without a working nested hypervisor.
assert vm.services.cloud-init.enable && vm.services.cloud-init.config == "";
assert vm.services.cloud-init.settings.ssh_deletekeys == false;
assert vm.services.cloud-init.settings.ssh_genkeytypes == [ ];
assert builtins.elem "ssh" vm.services.cloud-init.settings.cloud_init_modules;
assert vm.services.openssh.generateHostKeys && hostKeys != [ ];
pkgs.testers.runNixOSTest {
  name = "atomics-vm-ssh-host-keys";

  nodes.machine = { config, ... }: {
    system.stateVersion = vm.system.stateVersion;
    users.users.${user}.isNormalUser = true;
    services.openssh = {
      inherit (vm.services.openssh) enable settings hostKeys generateHostKeys;
    };
    services.cloud-init = {
      inherit (vm.services.cloud-init) enable settings config;
    };

    # NoCloud accepts the same metadata/user-data format as Lima's cidata disk. Write it to its
    # seed directory before discovery, keeping the rest of the image's cloud-init configuration.
    systemd.services.test-cidata = {
      requiredBy = [ "cloud-init-local.service" ];
      before = [ "cloud-init-local.service" ];
      serviceConfig.Type = "oneshot";
      script = ''
        seed=/var/lib/cloud/seed/nocloud
        install -d -m 0700 "$seed"
        instance_id=$(cat /proc/sys/kernel/random/boot_id)
        printf 'instance-id: %s\n' "$instance_id" > "$seed/meta-data"
        cat > "$seed/user-data" <<EOF
        #cloud-config
        users:
          - name: ${user}
            ssh_authorized_keys:
              - ${snakeOilPublicKey} boot-$instance_id
        EOF
      '';
    };

    # Exercise a real NixOS generation activation on the same disk, as a VM update does.
    specialisation.updated.configuration.environment.etc."host-key-update".text = "updated\n";
    environment.systemPackages = [ config.services.openssh.package ];
  };

  testScript = ''
    def ready():
        machine.wait_for_unit("cloud-init.service")
        machine.wait_for_unit("sshd.service")
        machine.wait_for_open_port(22)
        # Prove cloud-init discovered this boot's new instance and ran the real ssh module.
        machine.succeed("test $(cat /var/lib/cloud/data/instance-id) = $(cat /proc/sys/kernel/random/boot_id)")
        machine.succeed("test -f /var/lib/cloud/instance/sem/config_ssh")
        machine.succeed("grep -F boot-$(cat /proc/sys/kernel/random/boot_id) /home/${user}/.ssh/authorized_keys")

    def keys():
        return machine.succeed("${fingerprints}")

    machine.start(allow_reboot=True)
    ready()
    original = keys()
    assert original.count("SHA256:") == ${toString (builtins.length hostKeys)}, original
    machine.log("Initial host key fingerprints: " + original)

    with subtest("preexisting host keys survive fresh instance metadata on reboot"):
        machine.reboot()
        ready()
        assert keys() == original, "SSH host keys changed after reboot"

    with subtest("host keys survive a system generation switch and another reboot"):
        previous = machine.succeed("readlink -f /run/current-system").strip()
        machine.succeed(f"{previous}/specialisation/updated/bin/switch-to-configuration test")
        assert machine.succeed("readlink -f /run/current-system").strip() != previous
        machine.succeed("test -f /etc/host-key-update")
        machine.succeed("systemctl restart sshd.service")
        machine.wait_for_open_port(22)
        assert keys() == original, "SSH host keys changed during generation activation"
        machine.reboot()
        ready()
        assert keys() == original, "SSH host keys changed after the second reboot"
  '';
}
