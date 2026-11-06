#!/usr/bin/env bash
# EXAMPLE VM check for nix/examples/dotnet-azure.nix, run by scripts/check-atomics-vm.sh from
# /etc/atomics/check.d: the .NET and Azure tools are there, and an amd64 SQL Server container
# starts under Rosetta.
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
container="atomics-vm-check-sql-${UID}-$$"
# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
  docker rm -f "$container" >/dev/null 2>&1 || true
  rm -f -- "$log"
}
trap cleanup EXIT

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
