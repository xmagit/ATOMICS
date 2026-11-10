# Model an existing guest account: activation retains dnsmasq's dynamically allocated UID.
# Check the installed rules with that passwd database, without the build-only sed substitution.
{ pkgs, vm }:

let
  inherit (pkgs) lib;
  rules = vm.networking.nftables.tables.atomics_egress.content;
  upstream = vm.atomics.vm.egress.upstream;
  passwd = pkgs.writeText "egress-upgrade-passwd" ''
    root:x:0:0:root:/root:/bin/sh
    dnsmasq:x:994:991:resolver:/var/empty:/bin/false
  '';
  groups = pkgs.writeText "egress-upgrade-group" ''
    root:x:0:
    dnsmasq:x:991:
  '';
  rulesFile = pkgs.writeText "egress-upgrade.nft" ''
    table inet atomics_egress {
      ${rules}
    }
  '';
in
assert vm.users.users.dnsmasq.uid == null;
assert upstream != "192.168.5.2";
assert lib.hasInfix ''ip daddr ${upstream} meta skuid "dnsmasq" udp dport 53 accept'' rules;
assert lib.hasInfix ''ip daddr ${upstream} meta skuid "dnsmasq" tcp dport 53 accept'' rules;
assert lib.hasInfix ''ip daddr 192.168.5.2 reject'' rules;
assert lib.hasInfix ''meta skuid { "root", "dnsmasq" } accept'' rules;
pkgs.runCommand "atomics-vm-egress-upgrade-test" {
  nativeBuildInputs = [ pkgs.nftables pkgs.getent ];
} ''
  export NSS_WRAPPER_PASSWD=${passwd}
  export NSS_WRAPPER_GROUP=${groups}
  export LD_PRELOAD=${pkgs.nss_wrapper}/lib/libnss_wrapper.so
  test "$(getent passwd dnsmasq | cut -d: -f3)" = 994
  export LD_PRELOAD="$LD_PRELOAD ${pkgs.lklWithFirewall.lib}/lib/liblkl-hijack.so"
  nft -c -f ${rulesFile}
  mkdir $out
''
