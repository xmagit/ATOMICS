#!/usr/bin/env bash
# Tests the EXAMPLE Azure DevOps steps (onboard.sh beside this file) in atomics-onboard, against stub
# tools, in a throwaway HOME:
#
#   onboard-test.sh ATOMICS_ONBOARD STEP_FILE GIT_HELPER [--journal] [DIR...]
#
# Claude, Codex and the GitHub CLI are stubbed as signed in; nix/vm/onboard-test.sh tests those. This
# checks that the first run signs in to az and the feeds, configures Git's Azure CLI helper and
# registers the pinned MCP server. Git's helper is the real script, with az stubbed: it must work
# non-interactively after a restart. A second run changes nothing, settings hold names and URLs
# only, and the canary stays in the tools' credential files (nix/vm/onboard-test-lib.sh).
set -euo pipefail

step_file=$(realpath "$2")
git_helper=$(realpath "$3")
# shellcheck source=nix/vm/onboard-test-lib.sh
source "${ONBOARD_TEST_LIB:-$(dirname -- "${BASH_SOURCE[0]}")/../../vm/onboard-test-lib.sh}" "$1" "${@:4}"

mkdir -p "$work/steps"
cp "$step_file" "$work/steps/50-azure-devops.sh"
export ATOMICS_ONBOARD_DIR=$work/steps
mcp_package=@azure-devops/mcp@2.11.0
# The Nix build sandbox has no /usr/bin/env: the copy runs under this shell's own bash.
sed "1s|^#!.*|#!$(command -v bash)|" "$git_helper" > "$stubs/git-credential-azure-cli"
chmod +x "$stubs/git-credential-azure-cli"

# ── Stub tools ───────────────────────────────────────────────────────────────

# Signed in; registers MCP servers in ~/.claude.json as Claude Code does.
stub claude <<'EOF'
config=${CLAUDE_CONFIG_DIR:-$HOME}/.claude.json
case "${1:-} ${2:-}" in
  "auth status") printf '{"loggedIn":true,"authMethod":"claude.ai"}\n' ;;
  "mcp add")
    shift 2
    [[ $1 == --scope && $2 == user ]] || exit 3
    name=$3
    [[ $4 == -- ]] || exit 3
    command=$5
    shift 5
    [[ -f $config ]] || printf '{}\n' >"$config"
    args=$(printf '%s\0' "$@" | jq -Rs 'split("\u0000")[:-1]')
    jq --arg name "$name" --arg command "$command" --argjson args "$args" \
      '.mcpServers[$name] = {type: "stdio", command: $command, args: $args, env: {}}' \
      "$config" >"$config.new"
    mv "$config.new" "$config"
    printf 'Added stdio MCP server %s to user config\n' "$name"
    ;;
  "mcp remove")
    [[ $3 == --scope && $4 == user ]] || exit 3
    [[ -f $config ]] && jq -e --arg name "$5" '.mcpServers[$name]' "$config" >/dev/null || exit 1
    jq --arg name "$5" 'del(.mcpServers[$name])' "$config" >"$config.new"
    mv "$config.new" "$config"
    ;;
  *) exit 3 ;;
esac
EOF

stub codex <<'EOF'
[[ "${1:-} ${2:-}" == "login status" ]] || exit 3
EOF

stub gh <<'EOF'
case "${1:-} ${2:-}" in
  "auth status") ;;
  "auth setup-git")
    git config --global --replace-all credential.https://github.com.helper ''
    git config --global --add credential.https://github.com.helper "!$0 auth git-credential"
    ;;
  *) exit 3 ;;
esac
EOF

stub az <<'EOF'
cache=$HOME/.azure/msal_token_cache.json
quiet=false
[[ " $* " == *" --output none "* ]] && quiet=true
case "${1:-} ${2:-}" in
  "account get-access-token")
    [[ " $* " == *" --resource 499b84ac-1321-427f-aa17-267ca6975798 "* ]] || exit 3
    if [[ -f $cache ]]; then
      if ! $quiet; then
        [[ " $* " == *" --query accessToken -o tsv "* ]] || exit 3
        case ${STUB_AZ_TOKEN_MODE:-valid} in
          valid) printf '%s\n' "$STUB_CANARY" ;;
          empty) ;;
          multiline) printf '%s\nextra\n' "$STUB_CANARY" ;;
          failed) exit 1 ;;
          *) exit 3 ;;
        esac
      fi
      exit 0
    fi
    printf "ERROR: Please run 'az login' to setup account.\n" >&2
    exit 1
    ;;
  "login --use-device-code")
    printf 'To sign in, use a web browser to open https://microsoft.com/devicelogin and enter the code ABCD1234.\n' >&2
    IFS= read -r token
    mkdir -p "${cache%/*}"
    printf '{"RefreshToken":"%s"}\n' "$token" >"$cache"
    $quiet || printf '[{"user":"%s"}]\n' "$STUB_CANARY"
    ;;
  *) exit 3 ;;
esac
EOF

# An old GCM configuration must be replaced, and must never see a credential.
stub git-credential-manager <<'EOF'
exit 99
EOF

stub CredentialProvider.Microsoft <<'EOF'
uri= noninteractive=false
while (($#)); do
  case $1 in
    -U) uri=$2; shift ;;
    -N) noninteractive=true ;;
    -V | -F) shift ;;
    -R) ;;
    *) exit 3 ;;
  esac
  shift
done
[[ ${ARTIFACTS_CREDENTIALPROVIDER_FORCE_CANSHOWDIALOG_TO:-} == false ]] || exit 4
cache=$HOME/.local/share/MicrosoftCredentialProvider/stub-$(printf '%s' "$uri" | cksum | cut -d' ' -f1)
if [[ ! -f $cache ]]; then
  $noninteractive && { printf 'No cached session token\n' >&2; exit 1; }
  printf '[CredentialProvider]DeviceFlow: %s\nTo sign in, use a web browser to open https://microsoft.com/devicelogin and enter the code FEED1234.\n' "$uri" >&2
  mkdir -p "${cache%/*}"
  printf '%s' "$STUB_CANARY" >"$cache"
fi
printf '{"Username":"VssSessionToken","Password":"%s"}\n' "$(cat "$cache")"
EOF

feed_a=https://pkgs.dev.azure.com/contoso/_packaging/main/nuget/v3/index.json
feed_b=https://contoso.pkgs.visualstudio.com/Platform/_packaging/tools/nuget/v3/index.json
items="claude codex gh az ado-git nuget ado-mcp private host"
settings_of() { printf '%s/.config/atomics/onboard/azure-devops.conf' "$1"; }
logins() { grep -cE '^(az login|claude mcp add|CredentialProvider.Microsoft .* -V Information$)' "$logs/$1.calls" || true; }
ado_helpers() { git config --global --get-all credential.https://dev.azure.com.helper | tr '\n' ,; }
gh_is_git_helper() { git config --global --add credential.https://github.com.helper '!gh auth git-credential'; }
credential_request() { printf 'protocol=https\nhost=%s\npath=contoso/Platform/_git/app\n\n' "${1:-dev.azure.com}"; }
fill_credential() { credential_request | GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=false SSH_ASKPASS=false git credential fill; }
# shellcheck disable=SC2329 # Called through expect.
cannot_fill_credential() { ! fill_credential >/dev/null 2>&1; }
# shellcheck disable=SC2329 # Called through expect.
has_password() { fill_credential | grep '^password=.' >/dev/null; }
credential_response() {
  printf 'protocol=https\nhost=dev.azure.com\npath=contoso/Platform/_git/app\nusername=AzureDevOps\npassword=%s\n\n' "$canary"
}

# ── 1. --check on a fresh VM: every item listed, nothing asked, nothing written ──

fresh_home vm
gh_is_git_helper
git_config=$(cat "$HOME/.gitconfig")
input=
run check-fresh --check
expect "fresh --check fails" test "$status" -eq 1
expect "fresh --check lists every item" test "$(all_states check-fresh)" \
  = "claude=ok codex=ok gh=ok az=missing ado-git=missing nuget=missing ado-mcp=missing private=ok host=skipped "
expect "fresh --check lists exactly nine lines" test "$(wc -l <"$logs/check-fresh")" -eq 9
expect "fresh --check signs in to nothing" test "$(logins check-fresh)" -eq 0
expect "fresh --check writes no settings" test ! -e "$(settings_of "$HOME")"
expect "fresh --check leaves Git's config alone" test "$(cat "$HOME/.gitconfig")" = "$git_config"
run help --help
expect "--help lists the step file's options" grep -q -- '--feed VALUE' "$logs/help"

# A helper configured before onboarding must not see Azure DevOps credentials.
git config --global credential.helper store
# The previous example's GCM configuration must migrate to Azure CLI as well.
git config --global --add credential.https://dev.azure.com.helper ''
git config --global --add credential.https://dev.azure.com.helper manager
git config --global credential.https://dev.azure.com.useHttpPath true
git config --global credential.https://dev.azure.com.msauthFlow devicecode

# ── 2. First run: every sign-in once ──────────────────────────────────────────

# What the person types at the az prompt.
printf -v input '%s\n' "$canary"
run first --org contoso --feed "$feed_a" --feed "$feed_b"
expect "first run succeeds (host skipped without a socket)" test "$status" -eq 0
expect "first run ends with every item ok" test "$(all_states first)" \
  = "claude=ok codex=ok gh=ok az=ok ado-git=ok nuget=ok ado-mcp=ok private=ok host=skipped "
expect "first run signs in to az with a device code" called first \
  "az login --use-device-code --allow-no-subscriptions --output none"
expect "first run signs in to both feeds" test "$(grep -c '^CredentialProvider.Microsoft -R -F Json -U .* -V Information$' "$logs/first.calls")" -eq 2
expect "first run registers the pinned Azure DevOps MCP server" called first \
  "claude mcp add --scope user azure-devops -- npx -y $mcp_package contoso --authentication azcli"
expect "the earlier credential helper stored nothing" test ! -e "$HOME/.git-credentials"
expect "Git replaces GCM with only the Azure CLI helper" test "$(ado_helpers)" = ",azure-cli,"
expect "Git uses the HTTP path" test "$(git config --global credential.https://dev.azure.com.useHttpPath)" = true
expect "Git does not configure a credential store" test -z "$(git config --global credential.credentialStore || true)"
expect "GCM is never called" test "$(grep -c '^git-credential-manager ' "$logs/first.calls" || true)" -eq 0
expect "Git requests the Azure DevOps token as TSV" called first \
  "az account get-access-token --resource 499b84ac-1321-427f-aa17-267ca6975798 --query accessToken -o tsv"
expect "the settings hold the organization and both feeds" test \
  "$(grep -v '^#' "$(settings_of "$HOME")" | tr '\n' ' ')" \
  = "organization=contoso nuget_feed=$feed_a nuget_feed=$feed_b "
expect "the settings are private" test "$(stat -c %a "$(settings_of "$HOME")")" = 600
expect "the token caches are private" test \
  "$(stat -c %a "$HOME/.azure" "$HOME/.azure/msal_token_cache.json" \
    "$HOME/.local/share/MicrosoftCredentialProvider" | tr '\n' ' ')" = "700 600 700 "

# ── 3. Second run: nothing to do ──────────────────────────────────────────────

input=
git_config=$(cat "$HOME/.gitconfig")
settings=$(cat "$(settings_of "$HOME")")
az_cache=$(cat "$HOME/.azure/msal_token_cache.json")
mcp_config=$(cat "$HOME/.claude.json")
run second
expect "second run succeeds" test "$status" -eq 0
expect "second run signs in to nothing" test "$(logins second)" -eq 0
expect "second run skips all eight finished steps" test "$(grep -c '^done before, skipped: ' "$logs/second")" -eq 8
expect "second run leaves Git's config as it was" test "$(cat "$HOME/.gitconfig")" = "$git_config"
expect "second run leaves settings as they were" test "$(cat "$(settings_of "$HOME")")" = "$settings"
expect "second run leaves the Azure CLI cache as it was" test "$(cat "$HOME/.azure/msal_token_cache.json")" = "$az_cache"
expect "second run leaves the MCP config as it was" test "$(cat "$HOME/.claude.json")" = "$mcp_config"

# A fresh process must fill credentials without a terminal, for any organization or agent.
credential=$(fill_credential </dev/null)
expect "Git fills a nonempty password without prompting" test \
  "$(printf '%s\n' "$credential" | grep '^password=')" = "password=$canary"
expect "the helper supplies a username" test \
  "$(printf '%s\n' "$credential" | grep '^username=')" = username=AzureDevOps
unset credential

# Neither storing nor erasing a Git credential calls az or saves a token.
calls_before=$(wc -l <"$STUB_CALLS")
expect "store emits nothing" test -z "$(credential_response | git-credential-azure-cli store)"
expect "erase emits nothing" test -z "$(credential_response | git-credential-azure-cli erase)"
credential_response | git credential approve
credential_response | git credential reject
expect "Git approve and reject leave earlier helpers unused" test ! -e "$HOME/.git-credentials"
expect "other hosts receive no token" test -z "$(credential_request example.com | git-credential-azure-cli get)"
expect "HTTP receives no token" test -z "$(printf 'protocol=http\nhost=dev.azure.com\n\n' | git-credential-azure-cli get)"
expect "store, erase and other URLs never call az" test "$(wc -l <"$STUB_CALLS")" -eq "$calls_before"
for mode in empty multiline failed; do
  export STUB_AZ_TOKEN_MODE=$mode
  expect "Git rejects a $mode Azure CLI token" cannot_fill_credential
  run "check-token-$mode" --check
  expect "the check rejects a $mode Azure CLI token" test "$(state_of "check-token-$mode" ado-git)" = missing
done
unset STUB_AZ_TOKEN_MODE

# Model a restart: only files on disk survive, with no credential-cache daemon or helper state.
mkdir -p "$work/restarted"
cp -r "$HOME/.azure" "$HOME/.config" "$work/restarted/"
cp "$HOME/.gitconfig" "$work/restarted/.gitconfig"
fresh_home restarted
expect "a restarted agent fills credentials using only az's disk cache" has_password
expect "no Git credential cache or GCM state is needed" test ! -e "$HOME/.cache"
expect "no Git or GCM tokens were written" test ! -e "$HOME/.local"
run check-restarted --check
expect "the Git check passes after restart" test "$(state_of check-restarted ado-git)" = ok
expect "restart requires no new sign-in" test "$(logins check-restarted)" -eq 0

mv "$HOME/.azure/msal_token_cache.json" "$work/az-cache"
expect "Git fails without an Azure CLI sign-in, without prompting" cannot_fill_credential
run check-signed-out --check
expect "the Git check fails without an Azure CLI sign-in" test "$(state_of check-signed-out ado-git)" = missing
mv "$work/az-cache" "$HOME/.azure/msal_token_cache.json"
export HOME=$work/vm

run check-done --check
expect "--check after onboarding passes" test "$status" -eq 0
expect "--check after onboarding lists every item" test "$(all_states check-done)" \
  = "claude=ok codex=ok gh=ok az=ok ado-git=ok nuget=ok ado-mcp=ok private=ok host=skipped "
chmod 755 "$HOME/.azure"
run check-open --check
expect "an open token cache folder is missing" test "$(state_of check-open private)" = missing
run fix-open
expect "a run makes it private again" test "$(stat -c %a "$HOME/.azure")" = 700

# ── 4. A changed organization, an unpinned registration, bad settings ────────

run new-org --org fabrikam
expect "a new organization re-registers the MCP server" called new-org \
  "claude mcp add --scope user azure-devops -- npx -y $mcp_package fabrikam --authentication azcli"
expect "a new organization keeps one MCP registration" test \
  "$(jq -c '.mcpServers["azure-devops"].args' "$HOME/.claude.json")" \
  = "[\"-y\",\"$mcp_package\",\"fabrikam\",\"--authentication\",\"azcli\"]"
expect "a new organization keeps the feeds" test "$(grep -c '^nuget_feed=' "$(settings_of "$HOME")")" -eq 2

claude mcp remove --scope user azure-devops
claude mcp add --scope user azure-devops -- npx -y @azure-devops/mcp fabrikam --authentication azcli >/dev/null
run check-unpinned --check
expect "an unpinned registration is not the pinned server" test "$(state_of check-unpinned ado-mcp)" = missing

run check-with-org --check --org contoso
expect "--check refuses to change settings" test "$status" -eq 2
run bad-org --org 'not an org!'
expect "an invalid organization is refused" test "$status" -eq 2
run bad-feed --feed "https://user:x@pkgs.dev.azure.com/contoso/nuget/v3/index.json"
expect "a feed URL with a user part is refused" test "$status" -eq 2

# ── 5. Settings found in ~/repos, through a terminal ──────────────────────────

if command -v script >/dev/null && script -qec true /dev/null >/dev/null 2>&1; then
  mkdir -p "$work/detect"
  cp -r "$HOME"/.azure "$HOME"/.local "$work/detect/"
  fresh_home detect
  git init -q "$HOME/repos/app"
  git -C "$HOME/repos/app" remote add origin "https://user:$canary@dev.azure.com/contoso/Platform/_git/app"
  mkdir -p "$HOME/repos/app/src"
  printf '<configuration><packageSources><add key="main" value="%s" /><add key="bad" value="https://user:%s@pkgs.dev.azure.com/x/nuget/v3/index.json" /></packageSources></configuration>\n' \
    "$feed_a" "$canary" >"$HOME/repos/app/src/NuGet.Config"
  : >"$STUB_CALLS"
  # Enter takes the detected organization, y the detected feed.
  printf '\ny\n' | script -qec "$onboard" /dev/null >"$logs/detect" 2>&1 || true
  expect "the organization comes from the Azure DevOps remote" \
    grep -qx organization=contoso "$(settings_of "$HOME")"
  expect "the feed comes from nuget.config, without the one with a user part" test \
    "$(grep '^nuget_feed=' "$(settings_of "$HOME")")" = "nuget_feed=$feed_a"
else
  printf 'SKIP settings found in ~/repos: no pseudo-terminal for script(1)\n'
fi

# ── 6. Where the canary is, and where it is not ───────────────────────────────

show_runs="first check-done"
finish '\.azure/msal_token_cache\.json|\.local/share/MicrosoftCredentialProvider/stub-[0-9]+|repos/app/(\.git/config|src/NuGet\.Config)' 3
