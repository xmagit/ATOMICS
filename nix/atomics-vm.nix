# The ATOMICS VM: a headless NixOS guest for Apple Virtualization (VZ) on Apple Silicon. ATOMICS
# stays native on the Mac and starts its agents and chief here over ssh (docs/nixos-vm.md).
#
# The base is generic: Docker with Rosetta for amd64 containers, Node.js, Git, the GitHub CLI,
# Claude Code, Codex, Rust and atomics-onboard. A team's own stack (SDKs, cloud CLIs, extra sign-ins,
# extra checks) comes from a module of its own; nix/examples/dotnet-azure.nix is an example.
{ config, lib, pkgs, modulesPath, ... }:

let
  inherit (lib) mkOption types;
  cfg = config.atomics.vm;
  stepFiles = lib.mapAttrs' (name: path: lib.nameValuePair "atomics/onboard.d/${name}.sh" { source = path; });
  checkFiles = lib.mapAttrs' (name: path: lib.nameValuePair "atomics/check.d/${name}.sh" { source = path; });
  home = "/home/${cfg.user}";
  hostSocket = "/run/atomics/atomics.sock";
  credentials = "${home}/.local/share/atomics/credentials";
  # The host starts in the first writable Mac folder, the repositories.
  hostFolder = lib.head (lib.filter (path: config.programs.atomics.macDesktop.shares.${path}.writable)
    (lib.attrNames config.programs.atomics.macDesktop.shares) ++ [ home ]);
  hostArgs = lib.concatStringsSep " " (map lib.escapeShellArg (
    [ "--remote-port" (toString cfg.host.port) ]
    ++ lib.optionals (cfg.host.workspace != null) [ "-w" cfg.host.workspace ]
    ++ cfg.host.projects));
in
{
  imports = [
    "${modulesPath}/profiles/qemu-guest.nix"
    ./vm/image.nix
    ./vm/hardened.nix
    ./vm/egress.nix
    ./vm/docker-images.nix
  ];

  options.atomics.vm = {
    user = mkOption {
      type = types.strMatching "[a-z_][a-z0-9_-]*";
      default = "atomics";
      description = ''
        The VM's login user (UID 1000). Agents, the chief and their sign-ins run as this user, and
        checkouts that stay in the VM go in its ~/repos.
      '';
    };

    macUser = mkOption {
      type = types.nullOr types.str;
      default = cfg.user;
      defaultText = lib.literalExpression "config.atomics.vm.user";
      example = "alice";
      description = "Optional build-time Mac account; null reads the account and shares from Lima cidata at boot.";
    };

    cpus = mkOption {
      type = types.ints.positive;
      default = 4;
      description = "Virtual CPUs in the Lima definition.";
    };

    memoryGiB = mkOption {
      type = types.ints.positive;
      default = 8;
      description = "Memory in the Lima definition, in GiB.";
    };

    diskGiB = mkOption {
      type = types.ints.positive;
      default = 100;
      description = "Size of the disk image and the Lima disk, in GiB. The image is sparse.";
    };

    onboardSteps = mkOption {
      type = types.attrsOf types.path;
      default = { };
      example = lib.literalExpression ''{ "50-my-registry" = ./onboard/my-registry.sh; }'';
      description = ''
        Extra atomics-onboard steps, by file name: bash files it sources from /etc/atomics/onboard.d
        in name order. nix/vm/onboard.sh describes what a step file defines.
      '';
    };

    host = {
      enable = lib.mkEnableOption ''
        the ATOMICS host in this VM (recommended). `atomics host` runs here as the VM's user: agents,
        the chief, hooks, MCP, git and builds all run in the VM, and ATOMICS on the Mac is only its
        window, a client over atomics.v1 through an ssh forward with a paired token (docs/nixos-vm.md)
      '';

      port = mkOption {
        type = types.port;
        default = 47800;
        description = "The host's protocol server port, on the guest's loopback only. The Mac's forward opens the same one.";
      };

      workspace = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "daily";
        description = "A saved workspace (a set of projects) the host opens: `atomics host -w <workspace>`.";
      };

      projects = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "/Users/alice/repos/app" ];
        description = "Projects the host opens, in the shared folders. Empty: the last session's, or none.";
      };
    };

    checks = mkOption {
      type = types.attrsOf types.path;
      default = { };
      example = lib.literalExpression ''{ "50-my-stack" = ./checks/my-stack.sh; }'';
      description = ''
        Extra checks for scripts/check-atomics-vm.sh, by file name: bash scripts it runs from
        /etc/atomics/check.d in name order, after its own. A check fails by exiting non-zero.
      '';
    };
  };

  config = {
    networking.hostName = "atomics-vm";
    system.stateVersion = "25.11";

    boot.loader.systemd-boot.enable = true;
    boot.loader.efi.canTouchEfiVariables = false;
    # Apple M4 and later report SME to guests of Apple Virtualization, and using it there crashes
    # programs with SIGILL (the .NET JIT in parallel restores and builds, for one:
    # https://github.com/dotnet/runtime/issues/122608). arm64.nosme hides it from the guest.
    boot.kernelParams = [ "console=hvc0" "arm64.nosme" ];
    boot.initrd.availableKernelModules = [ "virtio_pci" "virtio_blk" "virtio_scsi" "virtiofs" ];
    boot.supportedFilesystems = [ "btrfs" ];
    fileSystems."/" = {
      device = "/dev/disk/by-label/atomics-root";
      fsType = "btrfs";
    };
    fileSystems."/boot" = {
      device = "/dev/disk/by-label/ATOMICSBOOT";
      fsType = "vfat";
    };
    fileSystems."/mnt/lima-cidata" = {
      device = "/dev/disk/by-label/cidata";
      fsType = "auto";
      options = [ "ro" ];
    };

    # The host must expose its Rosetta VirtioFS share. The NixOS module mounts
    # it and registers x86_64 ELF binaries with binfmt's F flag for containers.
    virtualisation.rosetta.enable = true;
    virtualisation.rosetta.mountTag = "vz-rosetta";
    # Hardened mode gives the user rootless Docker instead (vm/hardened.nix).
    virtualisation.docker.enable = !cfg.hardened;

    users.users.${cfg.user} = {
      isNormalUser = true;
      inherit home;
      createHome = true;
      uid = 1000;
      extraGroups = lib.optionals (!cfg.hardened) [ "wheel" "docker" ];
    };
    systemd.tmpfiles.rules = [
      "d /home/${cfg.user}/repos 0755 ${cfg.user} users - -"
      "L+ /bin/bash - - - - /run/current-system/sw/bin/bash"
    ];
    # Keys only: Lima's key, imported by cloud-init, is the one way in. No root login.
    services.openssh = {
      enable = true;
      settings = {
        PasswordAuthentication = false;
        KbdInteractiveAuthentication = false;
        PermitRootLogin = "no";
      };
    };
    # Without hardened mode the VM is one trust domain (docs/nixos-vm.md, Trust and secrets): agents may
    # use sudo and Docker, so either is as good as root here. With it only the admin user is in wheel.
    security.sudo.wheelNeedsPassword = false;

    # Lima supplies only the SSH public key at boot. Credentials and user state
    # stay on the writable guest disk, outside the flake and Nix store.
    services.cloud-init = {
      enable = true;
      settings = {
        datasource_list = [ "NoCloud" ];
        users = [ cfg.user ];
        disable_root = true;
        preserve_hostname = true;
        cloud_init_modules = [ "seed_random" "users-groups" "ssh" ];
        cloud_config_modules = [ ];
        cloud_final_modules = [ ];
      };
    };
    systemd.services.lima-ready = {
      wantedBy = [ "multi-user.target" ];
      requires = [ "cloud-init.service" "atomics-mac-shares.service" ];
      # `limactl start` returns once the Mac's folders are mounted too.
      after = [ "cloud-init.service" "atomics-mac-shares.service" ];
      unitConfig.RequiresMountsFor = "/mnt/lima-cidata";
      serviceConfig.Type = "oneshot";
      script = ''
        instance_id=$(sed -n 's/^LIMA_CIDATA_IID=//p' /mnt/lima-cidata/lima.env | tr -d '"')
        test -n "$instance_id"
        printf '%s\n' "$instance_id" > /run/lima-ssh-ready
        printf '%s\n' "$instance_id" > /run/lima-boot-done
      '';
    };

    nixpkgs.config.allowUnfreePredicate = pkg:
      builtins.elem (lib.getName pkg) [ "claude-code" "codex" ];

    environment.systemPackages = with pkgs; [
      nodejs
      git
      gh
      # The default [editor] command, hx: with the host in the VM, the editor panel runs it here.
      helix
      claude-code
      codex
      (lib.hiPrio cargo)
      (lib.hiPrio rustc)
      rustup
      gcc
      pkg-config
      python3
      docker
      btrfs-progs
      # Sign-ins happen in the guest: run `atomics-onboard` there (docs/nixos-vm.md). Its test,
      # nix/vm/onboard-test.sh, also runs there and needs jq.
      (pkgs.callPackage ./vm/onboard.nix { })
      jq
    ];
    environment.etc = stepFiles cfg.onboardSteps // checkFiles cfg.checks;

    programs.atomics.enable = true;
    programs.atomics.user = cfg.user;
    # The VM is the agents' boundary: they run bare here, as with the Mac's wrappers. Seeded only
    # when the user has no config yet.
    programs.atomics.settings = lib.mkIf cfg.host.enable {
      isolation.mode = "bare";
      # An explicit agents list replaces the defaults, so keep Claude and the shell too.
      # Codex needs the worktree's git metadata and the host's queue socket outside its inner sandbox.
      agents = [
        { name = "claude"; command = [ "claude" ]; kind = "claude"; }
        {
          name = "codex";
          command = [ "codex" "--dangerously-bypass-approvals-and-sandbox" ];
          kind = "generic";
          busy_text = "esc to interrupt";
        }
        { name = "shell"; command = [ ]; kind = "shell"; }
      ];
    };

    # The ATOMICS host (atomics.vm.host): `atomics host` in a login shell of the VM's user, so agents get
    # the PATH and variables an interactive session has, in the Mac's shared folders. Its socket and
    # its sign-ins are this VM's; the protocol server listens on the guest's loopback only.
    environment.sessionVariables = lib.mkIf cfg.host.enable {
      ATOMICS_SOCKET = hostSocket;
      ATOMICS_CREDENTIALS_DIR = credentials;
    };
    systemd.services.atomics-host = lib.mkIf cfg.host.enable {
      description = "ATOMICS host (agents, chief, hooks and git for the Mac's ATOMICS)";
      wantedBy = [ "multi-user.target" ];
      requires = [ "atomics-mac-shares.service" ];
      after = [ "atomics-mac-shares.service" "network.target" ];
      environment = {
        ATOMICS_SOCKET = hostSocket;
        ATOMICS_CREDENTIALS_DIR = credentials;
        HOME = home;
      };
      serviceConfig = {
        User = cfg.user;
        Group = "users";
        WorkingDirectory = if cfg.macUser == null then home else hostFolder;
        RuntimeDirectory = "atomics";
        RuntimeDirectoryMode = "0700";
        ExecStart = "${pkgs.bash}/bin/bash --login ${pkgs.writeShellScript "atomics-host-start" ''
          set -e
          ${lib.optionalString (cfg.macUser == null) ''
            . /run/atomics-mac-desktop.env
            cd "$ATOMICS_HOST_FOLDER"
          ''}
          exec atomics host ${hostArgs}
        ''}";
        Restart = "on-failure";
        RestartSec = 5;
        TimeoutStopSec = 30;
      };
    };

    image.repart.imageSize = "${toString cfg.diskGiB}G";

    # ATOMICS runs natively on the Mac and starts agents and the chief here (docs/nixos-vm.md).
    programs.atomics.macDesktop = {
      enable = true;
      user = if cfg.macUser == null then "@ATOMICS_MAC_USER@" else cfg.macUser;
      runtimeIdentity = cfg.macUser == null;
      hostPort = if cfg.host.enable then cfg.host.port else null;
      lima.settings = {
        minimumLimaVersion = "2.2.0";
        vmType = "vz";
        arch = "aarch64";
        cpus = cfg.cpus;
        memory = "${toString cfg.memoryGiB}GiB";
        disk = "${toString cfg.diskGiB}GiB";
        # `limactl create --set` replaces this with the built image's absolute path.
        images = [ { location = "./atomics-vm.raw"; arch = "aarch64"; } ];
        # NixOS owns the Rosetta registration, including fix-binary for Docker containers.
        vmOpts.vz.rosetta = { enabled = true; binfmt = false; };
        containerd = { system = false; user = false; };
        ssh.loadDotSSHPubKeys = false;
      };
    };
  };
}
