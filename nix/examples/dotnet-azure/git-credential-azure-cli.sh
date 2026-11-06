#!/usr/bin/env bash
# Git's credential protocol: use Azure CLI's persistent sign-in for Azure DevOps HTTPS requests.
set -euo pipefail

case ${1:-} in
  get)
    protocol='' host=''
    while IFS='=' read -r key value && [[ -n $key ]]; do
      case $key in
        protocol) protocol=$value ;;
        host) host=$value ;;
      esac
    done
    [[ $protocol == https && $host == dev.azure.com ]] || exit 0
    token=$(az account get-access-token --resource 499b84ac-1321-427f-aa17-267ca6975798 \
      --query accessToken -o tsv) || exit 1
    [[ -n $token && $token != *$'\n'* && $token != *$'\r'* ]] || exit 1
    printf 'username=AzureDevOps\npassword=%s\n\n' "$token"
    ;;
  store | erase)
    # Azure CLI owns the refresh tokens; Git has nothing to save or erase here.
    cat >/dev/null
    ;;
  *) exit 1 ;;
esac
