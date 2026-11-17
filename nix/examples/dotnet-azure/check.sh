#!/usr/bin/env bash
# EXAMPLE VM check for nix/examples/dotnet-azure.nix, run by scripts/check-atomics-vm.sh from
# /etc/atomics/check.d: the .NET and Azure tools are there, a net8.0 test project's test host runs on
# .NET 8, and an amd64 SQL Server container starts under Rosetta.
set -euo pipefail

for tool in dotnet pwsh git-credential-azure-cli az CredentialProvider.Microsoft; do
  command -v "$tool"
done
test -f "${NUGET_PLUGIN_PATHS:?NUGET_PLUGIN_PATHS is unset}"
az version
# This must work from the immutable package, without a dynamic extension install or pip.
AZURE_EXTENSION_USE_DYNAMIC_INSTALL=no az repos --help >/dev/null

dotnet --list-sdks
dotnet --list-runtimes
dotnet --list-sdks | grep -q '^9\.'
dotnet --list-runtimes | grep -q '^Microsoft.NETCore.App 8\.'
dotnet --list-runtimes | grep -q '^Microsoft.AspNetCore.App 8\.'

log=$(mktemp)
tests=$(mktemp -d)
container="atomics-vm-check-sql-${UID}-$$"
# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
  docker rm -f "$container" >/dev/null 2>&1 || true
  rm -rf -- "$log" "$tests"
}
trap cleanup EXIT

# A net8.0 test project's test host runs on .NET 8, with no DOTNET_ROLL_FORWARD. The second run puts
# the test on an MSBuild worker node, as `dotnet test` of a solution does for some projects, from an
# empty environment like a service's: that node's dotnet must be the combined one, not the SDK's own
# (dotnetSdk in nix/examples/dotnet-azure.nix). Packages come from nuget.org into the scratch folder.
cat > "$tests/nuget.config" <<'EOF'
<configuration>
  <packageSources>
    <clear />
    <add key="nuget.org" value="https://api.nuget.org/v3/index.json" />
  </packageSources>
</configuration>
EOF
cat > "$tests/Net8Check.csproj" <<'EOF'
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
  </PropertyGroup>
  <ItemGroup>
    <PackageReference Include="Microsoft.NET.Test.Sdk" Version="17.12.0" />
    <PackageReference Include="xunit" Version="2.9.2" />
    <PackageReference Include="xunit.runner.visualstudio" Version="2.8.2" />
  </ItemGroup>
</Project>
EOF
cat > "$tests/RuntimeTest.cs" <<'EOF'
public class RuntimeTest
{
    [Xunit.Fact]
    public void RunsOnDotnet8() => Xunit.Assert.Equal(8, System.Environment.Version.Major);
}
EOF
NUGET_PACKAGES="$tests/packages" dotnet test "$tests/Net8Check.csproj" --disable-build-servers --nologo
env -i HOME="$HOME" PATH=/run/current-system/sw/bin NUGET_PACKAGES="$tests/packages" \
  MSBUILDNOINPROCNODE=1 dotnet test "$tests/Net8Check.csproj" --no-build --disable-build-servers --nologo
printf 'net8.0 test host: ran on .NET 8, also on an MSBuild worker node\n'

# A fresh runtime-only password, never written to the flake or Nix store.
MSSQL_SA_PASSWORD="Atomics-$(head -c 24 /dev/urandom | base64)-1a!"
export MSSQL_SA_PASSWORD
docker run -d --name "$container" --platform linux/amd64 \
  --cpus 2 --memory 4g \
  --env ACCEPT_EULA=Y --env MSSQL_PID=Developer --env MSSQL_SA_PASSWORD \
  mcr.microsoft.com/mssql/server:2022-latest
unset MSSQL_SA_PASSWORD

for ((attempt = 0; attempt < 240; attempt++)); do
  docker logs "$container" > "$log" 2>&1
  if grep -q 'SQL Server is now ready for client connections' "$log"; then
    grep 'SQL Server is now ready for client connections' "$log"
    docker inspect --format 'SQL image: {{.Image}}' "$container"
    printf '.NET and Azure example checks passed\n'
    exit 0
  fi
  if test "$(docker inspect --format '{{.State.Running}}' "$container")" != true; then
    cat "$log"
    exit 1
  fi
  sleep 1
done
cat "$log"
printf 'SQL Server did not become ready within 240 seconds\n' >&2
exit 1
