# EXAMPLE, off by default: a .NET and Azure DevOps stack for the ATOMICS VM (nix/atomics-vm.nix).
# It shows how a team adds its own toolchain, sign-ins and checks to the generic VM. Turn it on as it
# is with `atomics.examples.dotnetAzure.enable = true;` (docs/nixos-vm.md), or copy and adapt it.
#
# When enabled it adds:
# - .NET SDK 9 with the .NET 8 and ASP.NET Core 8 runtimes, PowerShell, Azure CLI with its Azure
#   DevOps extension, a Git helper using az and the Azure Artifacts credential provider, which
#   NuGet finds through NUGET_PLUGIN_PATHS;
# - atomics-onboard steps for az, Git push to Azure DevOps, the Azure Artifacts NuGet feeds and the
#   Azure DevOps MCP server for the chief, pinned to one release (dotnet-azure/onboard.sh);
# - a VM check for those tools and an amd64 SQL Server container under Rosetta (dotnet-azure/check.sh).
{ config, lib, pkgs, ... }:

let
  cfg = config.atomics.examples.dotnetAzure;

  # Backport Fabric's Invoke 3 fix, keeping the cached Python/Paramiko graph.
  # https://github.com/fabric/fabric/issues/2376
  # Keep this package set private to Azure CLI.
  # Skip the rebuilt packages' upstream tests; the VM check runs `az version`.
  azurePkgs = pkgs.extend (_final: prev: {
    pythonPackagesExtensions = prev.pythonPackagesExtensions ++ [
      (_pyFinal: pyPrev: {
        buildPythonPackage = args:
          let package = pyPrev.buildPythonPackage args;
          in if lib.hasPrefix "azure-cli" (package.pname or "")
            then package.overridePythonAttrs { doCheck = false; }
            else package;
        fabric = pyPrev.fabric.overridePythonAttrs (old: {
          patches = (old.patches or [ ]) ++ [
            (pkgs.fetchurl {
              url = "https://github.com/fabric/fabric/commit/7fa309db9dd5dac69079c1fb83b6f579555f7d53.patch";
              hash = "sha256-Y6IujOEvoNRstQrq4ZfdBjqkd1hI6lGYDhFbae9iAGI=";
            })
          ];
          pythonRelaxDeps = [ "invoke" ];
          doCheck = false;
          meta = old.meta // { broken = false; };
        });
      })
    ];
  });
  azureCli = azurePkgs.azure-cli.withExtensions [ azurePkgs.azure-cli-extensions.azure-devops ];

  # .NET SDK 9 with the .NET 8 runtimes, so net8.0 projects build, run and test on .NET 8.
  # combinePackages links share/dotnet/sdk/<version> to the SDK's own package, and the .NET host
  # follows links to dotnet.dll, so the SDK's base folder is in that package. MSBuild starts its
  # worker nodes with the dotnet two folders above that base: the SDK package's own, which finds
  # only the .NET 9 runtime. A test MSBuild runs on a worker node (as `dotnet test` of a solution
  # does for some of its projects) then gets a test host that cannot find .NET 8. Real copies of
  # dotnet.dll and MSBuild.dll keep the base folder, and so the worker nodes' dotnet, in the combined
  # package. The fix belongs in nixpkgs' combinePackages (docs/nixos-vm.md, Example: .NET and Azure
  # DevOps).
  dotnetSdk = (pkgs.dotnetCorePackages.combinePackages (with pkgs.dotnetCorePackages; [
    sdk_9_0
    runtime_8_0
    aspnetcore_8_0
  ])).overrideAttrs (wrapper: {
    src = wrapper.src.overrideAttrs (combined: {
      postBuild = combined.postBuild + ''
        for sdk in "$out"/share/dotnet/sdk/*; do
          target=$(readlink -f "$sdk")
          rm "$sdk"
          mkdir "$sdk"
          (shopt -s dotglob; ln -s "$target"/* "$sdk"/)
          for app in dotnet.dll MSBuild.dll; do
            rm "$sdk/$app"
            cp "$target/$app" "$sdk/$app"
          done
        done
      '';
    });
  });
  adoGitHelper = pkgs.writeShellApplication {
    name = "git-credential-azure-cli";
    runtimeInputs = [ azureCli pkgs.coreutils ];
    text = lib.removePrefix "#!/usr/bin/env bash\n"
      (builtins.readFile ./dotnet-azure/git-credential-azure-cli.sh);
  };
in
{
  options.atomics.examples.dotnetAzure.enable = lib.mkEnableOption ''
    the example .NET and Azure DevOps stack (nix/examples/dotnet-azure.nix): SDKs, Azure tools, their
    atomics-onboard steps and a VM check
  '';

  config = lib.mkIf cfg.enable {
    environment.systemPackages = with pkgs; [
      dotnetSdk
      powershell
      azureCli
      adoGitHelper
      azure-artifacts-credprovider
    ];
    environment.variables.NUGET_PLUGIN_PATHS =
      "${pkgs.azure-artifacts-credprovider}/lib/azure-artifacts-credprovider/CredentialProvider.Microsoft.dll";

    atomics.vm.onboardSteps."50-azure-devops" = ./dotnet-azure/onboard.sh;
    atomics.vm.checks."50-dotnet-azure" = ./dotnet-azure/check.sh;
  };
}
