{ pkgs, vm, package }:
let
  inherit (pkgs) lib;
  service = vm.systemd.services.atomics-secret-broker.serviceConfig;
  rules = vm.networking.nftables.tables.atomics_egress.content;
  probe = "${pkgs.python3}/bin/python3 ${./secret-broker-probe.py}";
in
assert service.User == "atomics-secret-broker";
assert service.StateDirectoryMode == "0700";
assert service.NoNewPrivileges;
assert lib.elem "atomics-secret-use" service.SupplementaryGroups;
assert lib.elem "atomics-secret-use" vm.users.users.${vm.atomics.vm.user}.extraGroups;
assert lib.elem "atomics-secret-use" vm.users.users.${vm.atomics.vm.adminUser}.extraGroups;
assert lib.elem "atomics-secret-use" vm.systemd.services.atomics-host.serviceConfig.SupplementaryGroups;
assert service.User != vm.atomics.vm.user;
assert !lib.elem "wheel" vm.users.users.${vm.atomics.vm.user}.extraGroups;
assert !lib.elem "docker" vm.users.users.${vm.atomics.vm.user}.extraGroups;
assert vm.users.users.atomics-secret-broker.isSystemUser;
assert lib.hasSuffix "/bin/nologin" (toString vm.users.users.atomics-secret-broker.shell);
assert vm.systemd.services.atomics-host.environment.ATOMICS_SECRET_BROKER_SOCKET == "/run/atomics-secret-broker/use.sock";
assert lib.hasInfix ''meta skuid "atomics-secret-broker" jump broker'' rules;
assert !lib.hasInfix ''meta skuid { "root", "dnsmasq", "atomics-secret-broker" } accept'' rules;
pkgs.testers.runNixOSTest {
  name = "atomics-secret-broker";
  # This check also runs on builders with no /dev/kvm, using QEMU's software emulation.
  requiredFeatures = { kvm = false; nixos-test = false; };
  globalTimeout = 1200;
  nodes = {
    machine = { lib, ... }: {
      imports = [ ../vm/secret-broker.nix ../vm/egress.nix ];
      options.atomics.vm = {
        user = lib.mkOption { type = lib.types.str; default = "agent"; };
        adminUser = lib.mkOption { type = lib.types.str; default = "admin"; };
        hardened = lib.mkOption { type = lib.types.bool; default = true; };
        host.enable = lib.mkOption { type = lib.types.bool; default = false; };
      };
      options.programs.atomics.package = lib.mkOption { type = lib.types.package; default = package; };
      config = {
        virtualisation = { memorySize = 1536; cores = 2; qemu.forceAccel = false; };
        users.users.agent = { isNormalUser = true; uid = 1000; };
        users.users.outsider = { isNormalUser = true; uid = 1002; };
        atomics.vm.secretBroker.enable = true;
        atomics.vm.egress = { enable = true; defaults = false; upstream = "192.168.1.2"; domains = [ "general.test" ]; };
        environment.systemPackages = [ package pkgs.python3 ];
      };
    };
    upstream = { pkgs, ... }: {
      virtualisation = { memorySize = 512; cores = 1; qemu.forceAccel = false; };
      networking.firewall.allowedUDPPorts = [ 53 ];
      networking.firewall.allowedTCPPorts = [ 443 ];
      systemd.services.test-dns = {
        wantedBy = [ "multi-user.target" ];
        serviceConfig.ExecStart = "${pkgs.python3}/bin/python3 ${./secret-broker-dns.py}";
      };
      systemd.services.test-upstream = {
        wantedBy = [ "multi-user.target" ];
        serviceConfig.ExecStart = "${pkgs.python3}/bin/python3 -m http.server 443 --bind 0.0.0.0";
      };
    };
  };
  testScript = ''
    start_all()
    machine.wait_for_unit("atomics-secret-broker.service")
    machine.wait_for_unit("atomics-secret-egress.path")
    upstream.wait_for_unit("test-dns.service")
    upstream.wait_for_open_port(443)
    machine.wait_until_succeeds("test -S /run/atomics-secret-broker/admin.sock")
    machine.succeed("test $(systemctl show -p User --value atomics-secret-broker) = atomics-secret-broker")
    machine.succeed("test $(stat -c %a /var/lib/atomics-secret-broker) = 700")
    machine.succeed("test $(stat -c %a /run/atomics-secret-broker/admin.sock) = 660")
    machine.succeed("test $(stat -c %a /run/atomics-secret-broker/use.sock) = 660")
    machine.succeed("test $(stat -c %G /run/atomics-secret-broker/use.sock) = atomics-secret-use")
    machine.succeed("runuser -u agent -- ${probe} private")
    machine.succeed("runuser -u outsider -- ${probe} use-private")
    machine.succeed("runuser -u agent -- ${probe} guess")
    machine.succeed("runuser -u outsider -- ${probe} forged-grant")
    # An address reachable under the agents' broad allowlist still gives the broker no access.
    machine.succeed("nft add element inet atomics_egress web4 '{ 192.168.1.2 }'")
    machine.succeed("runuser -u agent -- ${probe} connect yes 192.168.1.2 443")
    machine.succeed("runuser -u atomics-secret-broker -- ${probe} connect no 192.168.1.2 443")
    machine.succeed("printf test-canary | runuser -u admin -- env ATOMICS_SECRET_BROKER_SOCKET=/run/atomics-secret-broker/use.sock atomics secret add KEY --host api.allowed.test --method GET --path-prefix /v1")
    machine.wait_until_succeeds("grep -q api.allowed.test /run/atomics-secret-egress/dnsmasq.conf")
    machine.wait_until_succeeds("runuser -u atomics-secret-broker -- ${probe} dns")
    machine.wait_until_succeeds("runuser -u atomics-secret-broker -- ${probe} connect yes 192.168.1.2 443")
    machine.succeed("runuser -u atomics-secret-broker -- ${probe} connect no 192.168.1.2 80")
    machine.succeed("runuser -u atomics-secret-broker -- ${probe} connect no 127.0.0.1 443")
    listing = machine.succeed("runuser -u admin -- env ATOMICS_SECRET_BROKER_SOCKET=/run/atomics-secret-broker/use.sock atomics secret list")
    assert "KEY" in listing and "GET" in listing and "/v1" in listing and "test-canary" not in listing
    machine.succeed("runuser -u admin -- env ATOMICS_SECRET_BROKER_SOCKET=/run/atomics-secret-broker/use.sock atomics secret rm KEY")
    machine.wait_until_succeeds("test ! -s /run/atomics-secret-egress/dnsmasq.conf")
    machine.succeed("runuser -u atomics-secret-broker -- ${probe} connect no 192.168.1.2 443")
    machine.succeed("! journalctl -u atomics-secret-broker --no-pager | grep -q test-canary")
  '';
}
