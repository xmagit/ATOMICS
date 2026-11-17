{ pkgs, vm, hardenedModule ? ../vm/hardened.nix }:
let
  inherit (pkgs) lib;
  image = pkgs.dockerTools.buildLayeredImage {
    name = "wrapper-test";
    tag = "latest";
    contents = [ pkgs.pkgsStatic.busybox ];
    config.Cmd = [ "${pkgs.pkgsStatic.busybox}/bin/busybox" "id" ];
  };
in
assert !lib.elem "wheel" vm.users.users.${vm.atomics.vm.user}.extraGroups;
assert !lib.elem "docker" vm.users.users.${vm.atomics.vm.user}.extraGroups;
assert vm.virtualisation.docker.rootless.enable;
pkgs.testers.runNixOSTest {
  name = "atomics-hardened-wrappers";
  nodes.machine = { lib, ... }: {
    imports = [ hardenedModule ];
    options = {
      atomics.vm = {
        user = lib.mkOption { type = lib.types.str; default = "agent"; };
        host.enable = lib.mkOption { type = lib.types.bool; default = false; };
      };
      programs.atomics.macDesktop = {
        home = lib.mkOption { type = lib.types.str; default = "/Users/test"; };
        adminUser = lib.mkOption { type = lib.types.str; };
        shares = lib.mkOption { type = lib.types.attrs; default = { }; };
        noexecShares = lib.mkOption { type = lib.types.bool; default = false; };
      };
    };
    config = {
      virtualisation = { memorySize = 2048; cores = 2; };
      atomics.vm = { hardened = true; adminUser = "maintainer"; };
      users.users.agent = {
        isNormalUser = true;
        uid = 1000;
        extraGroups = vm.users.users.${vm.atomics.vm.user}.extraGroups;
      };
      security.sudo.wheelNeedsPassword = false;
      # The test has no Lima cidata or SSH keys to provision.
      systemd.services.atomics-harden.enable = false;
      systemd.services.lima-ready.enable = false;
      environment.systemPackages = [ pkgs.python3 pkgs.util-linux ];
    };
  };
  testScript = ''
    start_all()
    machine.wait_for_unit("user@1000.service")
    machine.wait_until_succeeds("runuser -u agent -- env XDG_RUNTIME_DIR=/run/user/1000 systemctl --user is-active docker.service")
    machine.succeed("runuser -u agent -- env XDG_RUNTIME_DIR=/run/user/1000 docker --host unix:///run/user/1000/docker.sock load --input ${image}")
    with subtest("VM user wrapper restrictions and rootless Docker"):
        print(machine.succeed("runuser -u agent -- bash ${./hardened-wrappers.sh} wrapper-test:latest"))
    with subtest("admin retains sudo"):
        print(machine.succeed("runuser -u maintainer -- bash ${./hardened-wrappers.sh} --admin"))
  '';
}
