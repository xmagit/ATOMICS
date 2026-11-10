# Model an existing guest account: activation retains dnsmasq's dynamically allocated UID.
# Check the installed rules with that passwd database, without the build-only sed substitution.
{ pkgs, vm }:

let
  inherit (pkgs) lib;
  rules = vm.config.networking.nftables.tables.atomics_egress.content;
  upstream = vm.config.atomics.vm.egress.upstream;
  upstreamType = vm.options.atomics.vm.egress.upstream.type;
  precedes = first: second:
    lib.hasInfix second rules && lib.hasInfix first (lib.head (lib.splitString second rules));
  established = "ct state established,related accept";
  dnsUdp = ''ip daddr ${upstream} meta skuid "dnsmasq" udp dport 53 accept'';
  dnsTcp = ''ip daddr ${upstream} meta skuid "dnsmasq" tcp dport 53 accept'';
  dhcp = ''ip daddr 192.168.5.2 meta skuid "root" udp dport 67 accept'';
  reject = "ip daddr 192.168.5.2 reject";
  exemptions = ''meta skuid { "root", "dnsmasq" } accept'';
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
assert vm.config.users.users.dnsmasq.uid == null;
assert upstream != "192.168.5.2";
assert precedes established dnsUdp && precedes dnsUdp dnsTcp && precedes dnsTcp dhcp;
assert precedes dhcp reject && precedes reject exemptions;
assert lib.all upstreamType.check [
  "192.168.5.2" "0.0.0.0" "255.255.255.255" "::" "::1" "2001:db8::53"
  "2001:db8:0:1:2:3:4:5" "::ffff:192.0.2.53"
];
assert lib.all (value: !upstreamType.check value) [
  "resolver.example" "localhost" "" "256.1.1.1" "01.2.3.4" "192.168.5.2/24"
  "192.168.5.2#53" "[::1]" "fe80::1%eth0" "2001:db8::/64" "1::2::3" ":::"
  "12345::1" "00000::1" "1:2:3:4:5:6:7" "1:2:3:4:5:6:7:8:9" "::ffff:999.1.1.1"
];
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
