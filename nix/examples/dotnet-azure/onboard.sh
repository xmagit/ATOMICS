# shellcheck shell=bash disable=SC2034,SC2154
# ($reason, $skip and $check_seconds belong to atomics-onboard, which sources this file.)
# EXAMPLE atomics-onboard steps for nix/examples/dotnet-azure.nix: Azure CLI, Git push to Azure
# DevOps, Azure Artifacts NuGet feeds and the Azure DevOps MCP server for the chief.
#
# atomics-onboard sources this file (see nix/vm/onboard.sh for the step API). The only file it
# writes holds the Azure DevOps organization and NuGet feed URLs, never a secret:
# ~/.config/atomics/onboard/azure-devops.conf.

readonly ado_resource=499b84ac-1321-427f-aa17-267ca6975798 # Azure DevOps's Microsoft Entra resource
readonly ado_url=https://dev.azure.com
readonly ado_mcp_name=azure-devops
# Pinned: npx runs exactly this release. Change it on purpose, after reading its release notes.
readonly ado_mcp_package=@azure-devops/mcp@2.11.0
readonly ado_org_pattern='^[A-Za-z0-9]([A-Za-z0-9-]{0,48}[A-Za-z0-9])?$'
readonly ado_feed_pattern='^https://(pkgs\.dev\.azure\.com|[A-Za-z0-9-]+\.pkgs\.visualstudio\.com)/[A-Za-z0-9._~%/-]+/nuget/v3/index\.json$'

ado_settings=${XDG_CONFIG_HOME:-$HOME/.config}/atomics/onboard/azure-devops.conf
ado_claude_config=$HOME/.claude.json
if [[ ${CLAUDE_CONFIG_DIR:-} == /* ]]; then
  ado_claude_config=$CLAUDE_CONFIG_DIR/.claude.json
fi

# ── Settings: an organization name and feed URLs, never a secret ─────────────

ado_org=
ado_feeds=()
ado_org_option=
ado_feed_options=()

ado_load_settings() {
  [[ -f $ado_settings ]] || return 0
  local line key value n=0
  while IFS= read -r line || [[ -n $line ]]; do
    n=$((n + 1))
    [[ $line =~ ^[[:space:]]*(#|$) ]] && continue
    key=${line%%=*}
    value=${line#*=}
    case $key in
      organization)
        if [[ $value =~ $ado_org_pattern ]]; then ado_org=$value; else warn "$ado_settings:$n: not an organization name; ignored"; fi
        ;;
      nuget_feed)
        if [[ $value =~ $ado_feed_pattern ]]; then ado_feeds+=("$value"); else warn "$ado_settings:$n: not an Azure Artifacts feed URL; ignored"; fi
        ;;
      *) warn "$ado_settings:$n: unknown setting; ignored" ;;
    esac
  done <"$ado_settings"
}

ado_save_settings() {
  local dir=${ado_settings%/*} tmp feed
  mkdir -p "$dir"
  chmod 700 "$dir"
  tmp=$(mktemp "$dir/.azure-devops.XXXXXX")
  {
    printf '# atomics-onboard settings for Azure DevOps: names and URLs only, never a secret.\n'
    [[ -z $ado_org ]] || printf 'organization=%s\n' "$ado_org"
    for feed in "${ado_feeds[@]}"; do printf 'nuget_feed=%s\n' "$feed"; done
  } >"$tmp"
  mv -f "$tmp" "$ado_settings"
}

# The organization of the first Azure DevOps remote in ~/repos. Only the name leaves: a remote
# URL can carry a credential.
ado_detect_org() {
  local repo url name
  for repo in "$HOME"/repos/*/; do
    [[ -e $repo.git ]] || continue
    while IFS= read -r url; do
      if [[ $url =~ ^https://([^/@]+@)?dev\.azure\.com/([^/]+)/ ]]; then
        name=${BASH_REMATCH[2]}
      elif [[ $url =~ ssh\.dev\.azure\.com[:/]v3/([^/]+)/ ]]; then
        name=${BASH_REMATCH[1]}
      elif [[ $url =~ ^https://([^/@]+@)?([A-Za-z0-9-]+)\.visualstudio\.com/ ]]; then
        name=${BASH_REMATCH[2]}
      else
        continue
      fi
      if [[ $name =~ $ado_org_pattern ]]; then
        printf '%s\n' "$name"
        return 0
      fi
    done < <(git -C "$repo" config --get-regexp '^remote\..*\.url$' 2>/dev/null | cut -d' ' -f2-)
  done
  return 0
}

# Azure Artifacts feed URLs named in the nuget.config files of ~/repos. A URL with a user part is
# never one: the pattern has no '@'.
ado_detect_feeds() {
  local file
  while IFS= read -r -d '' file; do
    grep -oE 'https://[^"<>[:space:]]+/nuget/v3/index\.json' "$file" 2>/dev/null || true
  done < <(find "$HOME/repos" -mindepth 2 -maxdepth 3 -iname nuget.config -print0 2>/dev/null) \
    | while IFS= read -r url; do
      if [[ $url =~ $ado_feed_pattern ]]; then printf '%s\n' "$url"; fi
    done \
    | sort -u
}

ado_take_org() {
  [[ $1 =~ $ado_org_pattern ]] || die "--org: not an Azure DevOps organization name"
  ado_org_option=$1
}
ado_take_feed() {
  [[ $1 =~ $ado_feed_pattern ]] || die "--feed: not an Azure Artifacts feed URL (https://pkgs.dev.azure.com/.../nuget/v3/index.json)"
  ado_feed_options+=("$1")
}

ado_resolve_settings() {
  local changed=false
  if [[ -n $ado_org_option ]]; then
    [[ $ado_org_option == "$ado_org" ]] || changed=true
    ado_org=$ado_org_option
  elif [[ -z $ado_org && -t 0 ]]; then
    local detected
    detected=$(ado_detect_org)
    while ask "Azure DevOps organization (dev.azure.com/<organization>)" "$detected"; do
      if [[ $REPLY =~ $ado_org_pattern ]]; then
        ado_org=$REPLY
        changed=true
        break
      fi
      warn "an organization name has letters, digits and '-' only"
    done
  fi

  if ((${#ado_feed_options[@]})); then
    ado_feeds=("${ado_feed_options[@]}")
    changed=true
  elif ((${#ado_feeds[@]} == 0)) && [[ -t 0 ]]; then
    local detected_feeds=()
    mapfile -t detected_feeds < <(ado_detect_feeds)
    if ((${#detected_feeds[@]})); then
      printf 'Azure Artifacts feeds in ~/repos:\n'
      printf '  %s\n' "${detected_feeds[@]}"
      if ask "Sign in to these feeds? (y/n)" y && [[ $REPLY == [Yy]* ]]; then
        ado_feeds=("${detected_feeds[@]}")
        changed=true
      fi
    fi
    if ((${#ado_feeds[@]} == 0)); then
      while ask "Azure Artifacts feed URL (.../nuget/v3/index.json; empty when done)" && [[ -n $REPLY ]]; do
        if [[ $REPLY =~ $ado_feed_pattern ]]; then
          ado_feeds+=("$REPLY")
          changed=true
        else
          warn "not an Azure Artifacts feed URL"
        fi
      done
    fi
  fi
  if $changed; then ado_save_settings; fi
}

# ── Items ─────────────────────────────────────────────────────────────────────

check_az() {
  installed az || { reason="az is not installed"; return 1; }
  quiet az account get-access-token --resource "$ado_resource" --output none \
    || { reason="not signed in, or no Azure DevOps token"; return 1; }
}
login_az() { az login --use-device-code --allow-no-subscriptions --output none; }

# Git asks Azure CLI for dev.azure.com only. The empty helper resets earlier helpers, so no other
# helper handles the token. Each request uses az's persistent sign-in, including after a VM restart.
ado_git_helpers() { git config --global --get-all "credential.$ado_url.helper" 2>/dev/null || true; }
ado_git_configured() {
  [[ $(ado_git_helpers) == $'\nazure-cli' ]] \
    && [[ $(git config --global --get "credential.$ado_url.useHttpPath" 2>/dev/null) == true ]]
}
ado_configure_git() {
  git config --global --replace-all "credential.$ado_url.helper" ''
  git config --global --add "credential.$ado_url.helper" azure-cli
  git config --global "credential.$ado_url.useHttpPath" true
}
ado_credential_request() { printf 'protocol=https\nhost=dev.azure.com\npath=%s\n\n' "$ado_org"; }
check_ado_git() {
  installed git-credential-azure-cli || { reason="git-credential-azure-cli is not installed"; return 1; }
  ado_git_configured || { reason="Azure CLI Git helper is not set up for dev.azure.com"; return 1; }
  [[ -n $ado_org ]] || { reason="no Azure DevOps organization set (atomics-onboard --org NAME)"; return 1; }
  # Prompts are off; only the presence of a nonempty password leaves the pipe, never its value.
  ado_credential_request | GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=false SSH_ASKPASS=false timeout "$check_seconds" \
    git credential fill 2>/dev/null | grep '^password=.' >/dev/null \
    || { reason="no Azure DevOps token from Azure CLI"; return 1; }
}
login_ado_git() { ado_configure_git; }

# The credential provider prints a credential on stdout, as JSON with -F Json (redacted with -R,
# and discarded all the same); its log, with any device code, goes to stderr. No dialog: device flow.
readonly ado_credential_provider=(env ARTIFACTS_CREDENTIALPROVIDER_FORCE_CANSHOWDIALOG_TO=false
  CredentialProvider.Microsoft -R -F Json)
# Without -I the provider answers from its session token cache, renewing silently when it can.
ado_feed_signed_in() { quiet "${ado_credential_provider[@]}" -U "$1" -N -V Error; }
check_nuget() {
  installed CredentialProvider.Microsoft || { reason="the Azure Artifacts credential provider is not installed"; return 1; }
  ((${#ado_feeds[@]})) || { reason="no feed set (atomics-onboard --feed URL)"; return 1; }
  local feed signed=0
  for feed in "${ado_feeds[@]}"; do
    if ado_feed_signed_in "$feed"; then signed=$((signed + 1)); fi
  done
  ((signed == ${#ado_feeds[@]})) || { reason="$signed of ${#ado_feeds[@]} feeds signed in"; return 1; }
}
login_nuget() {
  local feed
  for feed in "${ado_feeds[@]}"; do
    ado_feed_signed_in "$feed" || "${ado_credential_provider[@]}" -U "$feed" -V Information >/dev/null || true
  done
}

# The chief is Claude Code in this VM; its user-scope servers are in ~/.claude.json.
ado_mcp_args() { jq -cn --arg org "$ado_org" --arg package "$ado_mcp_package" '["-y", $package, $org, "--authentication", "azcli"]'; }
check_ado_mcp() {
  [[ -n $ado_org ]] || { reason="no Azure DevOps organization set (atomics-onboard --org NAME)"; return 1; }
  [[ -f $ado_claude_config ]] || { reason="not registered"; return 1; }
  jq -e --arg name "$ado_mcp_name" --argjson args "$(ado_mcp_args)" \
    '.mcpServers[$name] | .command == "npx" and .args == $args' "$ado_claude_config" >/dev/null 2>&1 \
    || { reason="not registered for this organization and version"; return 1; }
}
login_ado_mcp() {
  installed claude || return 1
  [[ -n $ado_org ]] || return 1
  # A registration for another organization or version is replaced.
  claude mcp remove --scope user "$ado_mcp_name" >/dev/null 2>&1 || true
  claude mcp add --scope user "$ado_mcp_name" -- npx -y "$ado_mcp_package" "$ado_org" --authentication azcli
}

onboard_option org "Azure DevOps organization (dev.azure.com/NAME); remembered" ado_take_org
onboard_option feed "Azure Artifacts feed, .../nuget/v3/index.json (repeatable); replaces the remembered ones" ado_take_feed
onboard_prepare ado_resolve_settings
onboard_step az "Azure CLI (az login, Azure DevOps token)"
onboard_step ado-git "Git push to Azure DevOps (Azure CLI token)"
onboard_step nuget "Azure Artifacts NuGet feeds (credential provider)"
onboard_step ado-mcp "Azure DevOps MCP server for the chief (az auth)"
# Keep token caches private, including any shared MSAL cache and the provider's session tokens.
onboard_private .azure .azure/msal_token_cache.json .local/.IdentityService .local/share/MicrosoftCredentialProvider
ado_load_settings
