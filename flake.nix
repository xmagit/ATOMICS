{
  description = "ATOMICS desktop application and NixOS module";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "aarch64-linux" "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      vm = self.nixosConfigurations.atomics-vm;
      # The VM with the example .NET and Azure DevOps stack turned on, so the example keeps building.
      vmWithExample = vm.extendModules {
        modules = [ { atomics.examples.dotnetAzure.enable = true; } ];
      };
      # The recommended layout: the ATOMICS host runs in the VM and the Mac only shows it.
      vmRelease = vm.extendModules {
        modules = [ { atomics.vm = { macUser = null; host.enable = true; }; } ];
      };
      vmWithHost = vm.extendModules {
        modules = [ { atomics.vm.host.enable = true; } ];
      };
      vmWithEgress = vm.extendModules {
        modules = [ { atomics.vm = { hardened = true; egress.enable = true; }; } ];
      };
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          unwrapped = pkgs.callPackage ./nix/atomics.nix { };
        in
        {
          atomics = pkgs.callPackage ./nix/atomics-versioned.nix {
            atomics = unwrapped;
            revision = self.shortRev or self.dirtyShortRev or null;
          };
          atomics-onboard = pkgs.callPackage ./nix/vm/onboard.nix { };
          default = self.packages.${system}.atomics;
        } // nixpkgs.lib.optionalAttrs (system == "aarch64-linux") {
          atomics-vm-release-image = vmRelease.config.system.build.image;
          atomics-vm-release-bundle = vmRelease.config.programs.atomics.macDesktop.bundle;
          atomics-vm-image = vm.config.system.build.image;
          # The Mac side of the VM: wrappers, socket forward, lima.yaml and config snippet.
          atomics-vm-mac = vm.config.programs.atomics.macDesktop.bundle;
        });

      # `nix develop .#default` keeps Cargo's toolchain and libraries, rather than the wrapper's.
      devShells = forAllSystems (system: {
        default = self.packages.${system}.atomics.unwrapped;
      });

      checks = forAllSystems (system:
        let
          onboardTest = args: nixpkgs.legacyPackages.${system}.callPackage ./nix/vm/onboard-test.nix ({
            atomics-onboard = self.packages.${system}.atomics-onboard;
          } // args);
        in
        {
          atomics-mac-shares = nixpkgs.legacyPackages.${system}.runCommand "atomics-mac-shares-test" {
            nativeBuildInputs = [ nixpkgs.legacyPackages.${system}.python3 nixpkgs.legacyPackages.${system}.bash ];
          } ''
            python3 ${./nix/tests/mac-shares-test.py} ${./nix/vm/mac-shares.sh}
            mkdir $out
          '';
          atomics = self.packages.${system}.atomics;
          atomics-onboard = onboardTest { };
          atomics-onboard-dotnet-azure = onboardTest {
            name = "atomics-onboard-dotnet-azure-test";
            script = ./nix/examples/dotnet-azure/onboard-test.sh;
            args = [
              "${./nix/examples/dotnet-azure/onboard.sh}"
              "${./nix/examples/dotnet-azure/git-credential-azure-cli.sh}"
            ];
          };
        } // nixpkgs.lib.optionalAttrs (system == "aarch64-linux") {
          atomics-vm = vm.config.system.build.toplevel;
          atomics-vm-dotnet-azure = vmWithExample.config.system.build.toplevel;
          atomics-vm-mac = import ./nix/tests/mac-desktop.nix {
            pkgs = import nixpkgs { inherit system; };
            vm = vm.config;
          };
          atomics-vm-generic = vmRelease.config.system.build.toplevel;
          atomics-vm-generic-mac = vmRelease.config.programs.atomics.macDesktop.bundle;
          atomics-vm-with-host = vmWithHost.config.system.build.toplevel;
          # Builds the enforcing ruleset too, including NixOS's nft --check validation.
          atomics-vm-egress = vmWithEgress.config.system.build.toplevel;
          atomics-vm-egress-upgrade = import ./nix/tests/vm-egress-upgrade.nix {
            pkgs = import nixpkgs { inherit system; };
            vm = (vmWithEgress.extendModules {
              modules = [ { atomics.vm.egress.upstream = "192.0.2.53"; } ];
            });
          };
          atomics-vm-config = import ./nix/tests/vm-config.nix {
            pkgs = import nixpkgs { inherit system; };
            vm = vm.config;
            vmWithHost = vmWithHost.config;
            vmRelease = vmRelease.config;
            desktop = self.nixosConfigurations.atomics-test.config;
          };
          atomics-vm-host = import ./nix/tests/vm-host.nix {
            pkgs = import nixpkgs { inherit system; };
            vm = vmWithHost.config;
          };
        });

      nixosModules.atomics = import ./nix/modules/atomics.nix { inherit self; };

      # Settings (atomics.vm.user, macUser, cpus, memoryGiB, diskGiB, onboardSteps, checks, and
      # atomics.examples.dotnetAzure.enable) are set with extendModules; see docs/nixos-vm.md.
      nixosConfigurations.atomics-vm = nixpkgs.lib.nixosSystem {
        system = "aarch64-linux";
        modules = [
          self.nixosModules.atomics
          ./nix/atomics-vm.nix
          ./nix/examples/dotnet-azure.nix
        ];
      };

      nixosConfigurations.atomics-test = nixpkgs.lib.nixosSystem {
        system = "aarch64-linux";
        modules = [
          self.nixosModules.atomics
          ({ lib, ... }: {
            boot.loader.grub.enable = false;
            fileSystems."/" = {
              device = "/dev/null";
              fsType = "ext4";
            };
            networking.hostName = "atomics-test";
            system.stateVersion = "25.11";
            users.users.atomics-test = {
              isNormalUser = true;
              home = "/home/atomics-test";
            };
            programs.atomics.enable = true;
            programs.atomics.user = "atomics-test";
            programs.atomics.settings = {
              base_ref = "main";
              layout.diff_split = "auto";
            };
          })
        ];
      };
    };
}
