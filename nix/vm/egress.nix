# An outbound allowlist for every non-exempt UID (docs/nixos-vm.md, Network policy), including agents,
# subordinate UIDs in user namespaces and Nix builders.
#
# dnsmasq becomes the guest's resolver. It forwards only listed domains, to Lima's resolver (which asks the
# Mac's, and so its VPN's), and refuses every other name, so a lookup cannot carry data out either. Each
# address it hands out for a listed domain goes into an nftables set, and nftables lets new connections
# through only to those addresses. Only root and dnsmasq are exempt, but resolve
# through the same dnsmasq. Without atomics.vm.hardened an agent can become root and lift all of this.
{ config, lib, pkgs, ... }:

let
  inherit (lib) mkOption types;
  cfg = config.atomics.vm.egress;
  broker = config.atomics.vm.secretBroker.enable;
  brokerRuntime = "/run/atomics-secret-egress";
  # Explicit names, never a UID range: nixbld users are system users too, but must be filtered.
  exemptUsers = [ "root" "dnsmasq" ];
  exemptNames = lib.concatMapStringsSep ", " builtins.toJSON exemptUsers;
  # Keep Mac services blocked even when DNS is forwarded to a separate upstream.
  gateway = "192.168.5.2";
  ipv4Octet = "(25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])";
  isIPv4 = value: builtins.match "${ipv4Octet}(\\.${ipv4Octet}){3}" value != null;
  isIPv6 = value:
    let
      tail = lib.last (lib.splitString ":" value);
      # The library parser handles hex IPv6; an embedded IPv4 tail occupies two hex groups.
      hex = if isIPv4 tail then lib.removeSuffix tail value + "0:0" else value;
    in
    lib.hasInfix ":" value
    && builtins.match "[0-9A-Fa-f:]+" hex != null
    && lib.all (part: builtins.stringLength part <= 4) (lib.splitString ":" hex)
    && (builtins.tryEval (builtins.deepSeq (lib.network.ipv6.fromString hex).address true)).success;
  # An address literal, or a CIDR prefix such as 10.0.0.0/8 or 2001:db8::/32.
  isAddress = value:
    let
      parts = lib.splitString "/" value;
      address = lib.head parts;
      prefix = lib.elemAt parts 1;
      limit = if isIPv4 address then 32 else 128;
    in
    (isIPv4 address || isIPv6 address)
    && (lib.length parts == 1
      || (lib.length parts == 2 && builtins.match "0|[1-9][0-9]{0,2}" prefix != null && lib.toInt prefix <= limit));
  upstreamFamily = if lib.hasInfix ":" cfg.upstream then "ip6" else "ip";
  table = "atomics_egress";
  rules = [ { name = "web"; ports = cfg.webPorts; domains = cfg.domains; } ]
    ++ lib.imap0 (i: rule: rule // { name = "rule${toString i}"; }) cfg.rules;
  domains = lib.unique (lib.concatMap (rule: rule.domains) rules);
  ports = list: lib.concatMapStringsSep ", " toString list;
  sets = rule: ''
    set ${rule.name}4 { type ipv4_addr; flags timeout; timeout 1d; }
    set ${rule.name}6 { type ipv6_addr; flags timeout; timeout 1d; }
  '';
  addresses = family: rule: lib.filter (address: lib.hasInfix ":" address == (family == "ip6")) (rule.addresses or [ ]);
  allowAddresses = family: rule: lib.optionalString (addresses family rule != [ ]) ''
    tcp dport { ${ports rule.ports} } ${family} daddr { ${lib.concatStringsSep ", " (addresses family rule)} } accept
  '';
  allow = rule: ''
    tcp dport { ${ports rule.ports} } ip daddr @${rule.name}4 accept
    tcp dport { ${ports rule.ports} } ip6 daddr @${rule.name}6 accept
  '' + allowAddresses "ip" rule + allowAddresses "ip6" rule;

  # What any agent in this VM needs: its own service, GitHub, the common package registries and the
  # documentation sites agents read. A team adds its own (cloud, tickets, databases) with `domains` and
  # `rules`. Each entry includes its subdomains.
  defaults = [
    # Claude Code and Codex
    "anthropic.com" "claude.ai" "claude.com" "openai.com" "chatgpt.com"
    # GitHub, its packages, release downloads and raw files
    "github.com" "githubusercontent.com" "githubassets.com" "ghcr.io"
    # Package registries and toolchains
    "registry.npmjs.org" "npmjs.com" "nodejs.org" "yarnpkg.com"
    "api.nuget.org" "globalcdn.nuget.org" "nuget.org"
    "builds.dotnet.microsoft.com" "dotnetcli.azureedge.net" "dotnetcli.blob.core.windows.net" "aka.ms"
    "crates.io" "static.rust-lang.org" "rust-lang.org" "docs.rs"
    "pypi.org" "pythonhosted.org"
    "docker.io" "docker.com" "mcr.microsoft.com"
    "nixos.org"
    # Packages and downloads that image builds fetch
    "deb.debian.org" "security.debian.org" "archive.ubuntu.com" "security.ubuntu.com" "ports.ubuntu.com"
    "packages.microsoft.com" "download.microsoft.com" "go.microsoft.com"
    # Documentation
    "learn.microsoft.com" "docs.microsoft.com" "devblogs.microsoft.com"
    "stackoverflow.com" "stackexchange.com" "sstatic.net" "superuser.com" "serverfault.com" "askubuntu.com"
    "developer.mozilla.org" "typescriptlang.org" "web.dev" "caniuse.com" "lit.dev" "playwright.dev"
  ];
in
{
  options.atomics.vm.egress = {
    enable = lib.mkEnableOption ''
      an outbound allowlist for all users except root and dnsmasq, including subordinate UIDs and Nix
      builders. Pair it with atomics.vm.hardened, or an agent can become root and lift it
    '';

    enforce = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Refuse connections outside the list. False only logs them ("atomics-egress refused" in the kernel log,
        `journalctl -k -g atomics-egress`), to learn what a team needs before enforcing. Names outside the list
        are refused by dnsmasq either way. New connections to Lima's host address are always refused
        except for resolver DNS and root's DHCP renewals.
      '';
    };

    defaults = mkOption {
      type = types.bool;
      default = true;
      description = "Allow ATOMICS's default domains: the agents' services, GitHub, package registries and documentation.";
    };

    domains = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "dev.azure.com" "visualstudio.com" ];
      description = "Domains, with their subdomains, the user may reach on `webPorts`. Definitions add up.";
    };

    webPorts = mkOption {
      type = types.listOf types.port;
      default = [ 80 443 ];
      description = "The ports `domains` are allowed on.";
    };

    rules = mkOption {
      type = types.listOf (types.submodule {
        options = {
          domains = mkOption {
            type = types.listOf types.str;
            default = [ ];
            description = "Domains, with their subdomains.";
          };
          addresses = mkOption {
            type = types.listOf ((types.addCheck types.str isAddress) // {
              description = "IPv4 or IPv6 address literal or CIDR prefix";
            });
            default = [ ];
            example = [ "10.1.2.3" "10.20.0.0/16" ];
            description = ''
              Addresses or CIDR prefixes allowed without a name, such as a service on a private network that is
              only known by its address. Lima's host address stays refused even if listed.
            '';
          };
          ports = mkOption {
            type = types.listOf types.port;
            description = "The TCP ports allowed to them.";
          };
        };
      });
      default = [ ];
      example = [
        { domains = [ "db.example.database.windows.net" ]; ports = [ 1433 ]; }
        { addresses = [ "10.1.2.3" ]; ports = [ 9200 ]; }
      ];
      description = "Other ports for some domains or addresses, such as a database's.";
    };

    upstream = mkOption {
      type = (types.addCheck types.str (value: isIPv4 value || isIPv6 value)) // {
        description = "IPv4 or IPv6 address literal (not a hostname, port, zone or CIDR prefix)";
      };
      default = gateway;
      description = "The resolver IP address listed names are forwarded to; defaults to Lima's gateway resolver.";
    };
  };

  config = lib.mkIf cfg.enable {
    warnings = lib.optional (!config.atomics.vm.hardened)
      "atomics.vm.egress without atomics.vm.hardened: any agent can become root and lift the network policy.";
    assertions = map (rule: {
      assertion = rule.domains != [ ] || rule.addresses != [ ];
      message = "atomics.vm.egress.rules: each rule needs domains or addresses.";
    }) cfg.rules;

    atomics.vm.egress.domains = lib.mkIf cfg.defaults defaults;

    services.dnsmasq = {
      enable = true;
      resolveLocalQueries = true;
      settings = {
        listen-address = [ "127.0.0.1" ];
        bind-interfaces = true;
        # Only the listed domains have a server; every other name is refused.
        no-resolv = true;
        conf-file = lib.mkIf broker [ "/etc/dnsmasq-conf.conf" "${brokerRuntime}/dnsmasq.conf" ];
        server = map (domain: "/${domain}/${cfg.upstream}") domains;
        nftset = lib.concatMap
          (rule: map (domain: "/${domain}/4#inet#${table}#${rule.name}4,6#inet#${table}#${rule.name}6") rule.domains)
          rules;
        domain-needed = true;
        bogus-priv = true;
        cache-size = 10000;
        # Answers are asked for again within the hour, which renews their addresses in the sets.
        max-cache-ttl = 3600;
      };
    };
    # dnsmasq alone in resolv.conf: glibc asks the next server after a refusal, and Lima's would answer.
    networking.dhcpcd.extraConfig = "nohook resolv.conf";
    # dnsmasq adds to sets that must exist first.
    systemd.services.dnsmasq = {
      after = [ "nftables.service" ];
      requires = [ "nftables.service" ];
    };

    # Root projects validated hostnames into resolver configuration. The broker never receives
    # CAP_NET_ADMIN; its UID is checked before the agents' broad domain/loopback allowances.
    systemd.tmpfiles.rules = lib.optionals broker [
      "d ${brokerRuntime} 0755 root root - -"
      "f ${brokerRuntime}/dnsmasq.conf 0644 root root - -"
    ];
    systemd.paths.atomics-secret-egress = lib.mkIf broker {
      wantedBy = [ "multi-user.target" ];
      pathConfig.PathChanged = "/var/lib/atomics-secret-broker/rule-hosts.json";
    };
    systemd.services.atomics-secret-egress = lib.mkIf broker {
      description = "Project secret rules into the broker network policy";
      wantedBy = [ "multi-user.target" ];
      requires = [ "atomics-secret-broker.service" "nftables.service" ];
      # This job synchronously stops/starts dnsmasq. An ordering edge to dnsmasq would
      # make that stop wait for this oneshot's start job, deadlocking both jobs.
      after = [ "atomics-secret-broker.service" "nftables.service" ];
      partOf = [ "nftables.service" ];
      path = [ pkgs.nftables pkgs.systemd ];
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${pkgs.python3}/bin/python3 ${./secret-egress.py} /var/lib/atomics-secret-broker/rule-hosts.json ${brokerRuntime}/dnsmasq.conf ${cfg.upstream} ${pkgs.writeText "atomics-egress-domains.json" (builtins.toJSON rules)}";
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ReadWritePaths = [ brokerRuntime ];
      };
    };

    networking.nftables.enable = true;
    # The build sandbox has no dnsmasq account. Substitute only in its disposable check copy;
    # the installed rules resolve account names against the guest's existing passwd at load time.
    networking.nftables.preCheckRuleset = ''
      sed -i 's/"dnsmasq"/65534/g; s/"atomics-secret-broker"/65533/g' ruleset.conf
    '';
    networking.nftables.tables.${table} = {
      family = "inet";
      content = ''
        ${lib.concatMapStrings sets rules}
        ${lib.optionalString broker ''
          set broker4 { type ipv4_addr; flags timeout; timeout 1d; }
          set broker6 { type ipv6_addr; flags timeout; timeout 1d; }
          chain broker {
            ct state established,related accept
            ip daddr 127.0.0.1 udp dport 53 accept
            ip daddr 127.0.0.1 tcp dport 53 accept
            ip daddr ${gateway} reject
            tcp dport 443 ip daddr @broker4 accept
            tcp dport 443 ip6 daddr @broker6 accept
            reject with icmpx type admin-prohibited
          }
        ''}
        chain output {
          type filter hook output priority filter; policy accept;
          ${lib.optionalString broker ''meta skuid "atomics-secret-broker" jump broker''}
          # Replies to Mac-initiated SSH/forwarded connections can target the gateway address.
          # Accept established flows first to preserve those sessions; new flows still face the rules below.
          ct state established,related accept
          # Lima exposes Mac services on this address, including Mac loopback listeners. Only the
          # resolver may open DNS connections to the configured upstream. Keep this before UID
          # exemptions and domain sets. Names handle dynamically allocated UIDs on upgraded guests.
          ${upstreamFamily} daddr ${cfg.upstream} meta skuid "dnsmasq" udp dport 53 accept
          ${upstreamFamily} daddr ${cfg.upstream} meta skuid "dnsmasq" tcp dport 53 accept
          # dhcpcd runs as root and unicasts lease renewals to Lima's DHCP server on the gateway.
          ip daddr ${gateway} meta skuid "root" udp dport 67 accept
          ip daddr ${gateway} reject with icmp type admin-prohibited
          meta skuid { ${exemptNames} } accept
          oifname "lo" accept
          ${lib.concatMapStrings allow rules}
          limit rate 20/second burst 50 packets log prefix "atomics-egress refused: " level info
          ${if cfg.enforce then ''
            meta l4proto tcp reject with tcp reset
            reject with icmpx type admin-prohibited
          '' else "accept"}
        }
      '';
    };
  };
}
