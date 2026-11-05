#!/usr/bin/env bash
# atomics-onboard: the sign-ins the ATOMICS chief and agents need, made inside the ATOMICS VM.
#
#   atomics-onboard           sign in to whatever is missing; steps already done are skipped
#   atomics-onboard --check   list every item as ok, missing or skipped, without values
#
# Secrets are typed into each tool's own prompt, in this VM. This script never reads, prints or
# stores one: checks discard the tools' output. See docs/nixos-vm.md, "Sign in inside the VM".
#
# Built in: Claude, Codex and the GitHub CLI, and last a check of the ATOMICS host. More steps come
# from the bash files in $ATOMICS_ONBOARD_DIR (default /etc/atomics/onboard.d, where the NixOS option
# atomics.vm.onboardSteps puts them), sourced in name order. A step file calls:
#
#   onboard_step NAME LABEL            add an item; it defines check_NAME and login_NAME ('-' becomes '_')
#   onboard_option NAME HELP HANDLER   accept --NAME VALUE or --NAME=VALUE; HANDLER gets VALUE
#   onboard_prepare FUNCTION           run FUNCTION before the sign-ins (never with --check)
#   onboard_private PATH...            credential files or folders, relative to $HOME, that must be
#                                      private: no group or other access to them or their folders
#                                      (the `private` item)
#
# check_NAME runs quietly and returns 0 when the item is done. Otherwise it sets $reason to a fixed
# text, never a value, and sets skip=true when the item does not apply here. login_NAME signs in.
# Step files may use quiet, installed, warn, die and ask. nix/examples/dotnet-azure/onboard.sh is an
# example.
set -euo pipefail
# Files the sign-ins create start private to this user (the `private` item checks the rest).
umask 077

# A check waits this long for a tool; a sign-in waits for the person.
readonly check_seconds=60
readonly name_pattern='^[a-z][a-z0-9-]*$'
steps_dir=${ATOMICS_ONBOARD_DIR:-/etc/atomics/onboard.d}

warn() { printf 'atomics-onboard: %s\n' "$*" >&2; }
die() {
  warn "$*"
  exit 2
}

# Runs a check: no input, no output, bounded in time. Only the exit status leaves it.
quiet() { timeout "$check_seconds" "$@" </dev/null >/dev/null 2>&1; }
installed() { command -v "$1" >/dev/null 2>&1; }

ask() {
  local prompt=$1 default=${2:-}
  if [[ -n $default ]]; then
    read -r -p "$prompt [$default]: " REPLY || return 1
    REPLY=${REPLY:-$default}
  else
    read -r -p "$prompt: " REPLY || return 1
  fi
}

# ── The step registry ─────────────────────────────────────────────────────────

items=()
declare -A labels=()
options=()
declare -A option_help=() option_handler=()
prepare_hooks=()

onboard_step() {
  (($# == 2)) || die "onboard_step takes a name and a label"
  [[ $1 =~ $name_pattern ]] || die "step name '$1': use lower-case letters, digits and '-'"
  [[ -z ${labels[$1]+set} ]] || die "step '$1' is defined twice"
  items+=("$1")
  labels[$1]=$2
}

onboard_option() {
  (($# == 3)) || die "onboard_option takes a name, a help text and a handler"
  [[ $1 =~ $name_pattern && $1 != check && $1 != help ]] || die "option name '$1' is not allowed"
  [[ -z ${option_handler[$1]+set} ]] || die "option --$1 is defined twice"
  options+=("$1")
  option_help[$1]=$2
  option_handler[$1]=$3
}

onboard_prepare() {
  (($# == 1)) || die "onboard_prepare takes a function name"
  prepare_hooks+=("$1")
}

private_paths=()
onboard_private() {
  (($# > 0)) || die "onboard_private takes paths relative to the home folder"
  local path
  for path; do
    [[ -n $path && $path != /* && /$path/ != */../* ]] || die "onboard_private: '$path' is not relative to the home folder"
    private_paths+=("$path")
  done
}

# ── Built-in items: a quiet check that sets $reason, and a sign-in ────────────

reason=
skip=false

check_claude() {
  installed claude || { reason="claude is not installed"; return 1; }
  local status method
  # The status names the account; it stays in this variable.
  if ! status=$(timeout "$check_seconds" claude auth status </dev/null 2>/dev/null); then
    reason="not signed in"
    return 1
  fi
  method=$(jq -r '.authMethod // empty' <<<"$status" 2>/dev/null) || method=
  case $method in
    "" | claude.ai | oauth_token) return 0 ;;
    *)
      reason="signed in without a Claude subscription"
      return 1
      ;;
  esac
}
login_claude() { claude auth login; }

check_codex() {
  installed codex || { reason="codex is not installed"; return 1; }
  quiet codex login status || { reason="not signed in"; return 1; }
}
login_codex() {
  codex login --device-auth && return 0
  warn "device code sign-in failed: ChatGPT allows it only when its security settings (a workspace admin's, for a business account) turn it on"
  warn "sign in through the browser instead: on the Mac, run bin/atomics-vm-codex-login from the VM's bundle (~/.local/share/atomics-vm by default), then run atomics-onboard again"
  return 1
}

# gh signs in to github.com and becomes Git's credential helper there. nixpkgs' gh is a wrapper,
# and `gh auth setup-git` writes the path of the program behind it, in the Nix store, which a
# garbage collection after an update removes. The helper names gh where PATH finds it instead.
readonly github_hosts=(https://github.com https://gist.github.com)
git_helpers() { git config --global --get-all "credential.$1.helper" 2>/dev/null; }
gh_is_git_helper() {
  git_helpers "${github_hosts[0]}" | grep -Eq '^!(/[^ ]*/)?gh auth git-credential$' &&
    ! git_helpers "${github_hosts[0]}" | grep -q '^!/nix/store/'
}
check_gh() {
  installed gh || { reason="gh is not installed"; return 1; }
  quiet gh auth status --hostname github.com || { reason="not signed in"; return 1; }
  gh_is_git_helper || { reason="not Git's credential helper for github.com"; return 1; }
}
login_gh() {
  if ! quiet gh auth status --hostname github.com; then
    gh auth login --hostname github.com --git-protocol https --web || return 1
  fi
  gh auth setup-git --hostname github.com
  local host gh_path
  gh_path=$(command -v gh)
  for host in "${github_hosts[@]}"; do
    git_helpers "$host" | grep -q '^!/nix/store/' || continue
    git config --global --unset-all "credential.$host.helper"
    git config --global --add "credential.$host.helper" ''
    git config --global --add "credential.$host.helper" "!$gh_path auth git-credential"
  done
}

# The ATOMICS host on the Mac, through a socket forwarded into the VM. No socket: nothing to check.
check_host() {
  installed atomics || { reason="atomics is not installed"; return 1; }
  local error
  # The error names the socket's path; it stays in this variable.
  if error=$(timeout 10 atomics ctl '{"cmd":"list"}' </dev/null 2>&1 >/dev/null); then
    return 0
  fi
  if [[ $error == *"(os error 2)"* ]]; then
    skip=true
    reason="no ATOMICS socket here"
    return 1
  fi
  reason="its socket does not answer"
  return 1
}
login_host() {
  warn "nothing to sign in here; the forward is optional and makes this VM as trusted as the Mac (docs/nixos-vm.md)"
  return 1
}

# Every credential file and folder that exists has no group or other permissions: each existing
# file and its folder, and each folder named. Not recursive. Sets $private_open to what is not.
private_open=()
find_open() {
  private_open=()
  local path target mode
  for path in "${private_paths[@]}"; do
    for target in "$HOME/$path" "$(dirname -- "$HOME/$path")"; do
      [[ -e $target && ($target == "$HOME/$path" || -f $HOME/$path) ]] || continue
      [[ $target != "$HOME" ]] || continue
      mode=$(stat -L -c %a -- "$target" 2>/dev/null) || continue
      ((8#$mode & 8#077)) || continue
      [[ " ${private_open[*]} " == *" $target "* ]] || private_open+=("$target")
    done
  done
}
check_private() {
  find_open
  ((${#private_open[@]} == 0)) && return 0
  # A fixed path in the home folder, never a value.
  reason="${private_open[0]#"$HOME/"} in the home folder is open to other users"
  ((${#private_open[@]} == 1)) || reason+=" (and $((${#private_open[@]} - 1)) more)"
  return 1
}
login_private() {
  find_open
  local target
  for target in "${private_open[@]}"; do
    chmod go= -- "$target" && printf 'made private: %s in the home folder\n' "${target#"$HOME/"}"
  done
}

onboard_step claude "Claude subscription (Claude Code login; includes claude.ai connectors)"
onboard_step codex "Codex (ChatGPT login)"
onboard_step gh "GitHub CLI and Git push to github.com (gh login)"
onboard_private .claude/.credentials.json .codex/auth.json .config/gh/hosts.yml

# ── Step files ────────────────────────────────────────────────────────────────

if [[ -d $steps_dir ]]; then
  for step_file in "$steps_dir"/*.sh; do
    [[ -f $step_file ]] || continue
    # shellcheck source=/dev/null
    source "$step_file"
  done
fi
unset step_file

onboard_step private "Credential files private to this user (no group or other access)"
onboard_step host "ATOMICS host (in this VM, or the Mac's over the socket forward)"

# ── Arguments ─────────────────────────────────────────────────────────────────

usage() {
  local name
  cat <<'EOF'
Usage: atomics-onboard [OPTION VALUE]...
       atomics-onboard --check

Signs in, inside this VM, to everything the ATOMICS chief and agents use. Steps already done are
skipped, so it is safe to run again.

  --check      report each item as ok, missing or skipped; no sign-in, no prompt, no values
EOF
  for name in "${options[@]}"; do
    printf '  --%s VALUE\n      %s\n' "$name" "${option_help[$name]}"
  done
  printf '\nItems:\n'
  for name in "${items[@]}"; do printf '  %-8s %s\n' "$name" "${labels[$name]}"; done
}

check=false
given=false
while (($#)); do
  case $1 in
    --check) check=true ;;
    -h | --help)
      usage
      exit 0
      ;;
    --*=*)
      name=${1%%=*}
      name=${name#--}
      [[ -n ${option_handler[$name]+set} ]] || { usage >&2; exit 2; }
      "${option_handler[$name]}" "${1#*=}"
      given=true
      ;;
    --*)
      name=${1#--}
      [[ -n ${option_handler[$name]+set} ]] || { usage >&2; exit 2; }
      (($# >= 2)) || die "--$name needs a value"
      "${option_handler[$name]}" "$2"
      given=true
      shift
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
  shift
done
if $check && $given; then
  die "--check reads the remembered settings; run without --check to change them"
fi

# Run on the Mac it would sign in there and change ~/.claude.json and ~/.gitconfig.
[[ $OSTYPE == linux* ]] || die "run this inside the ATOMICS VM: limactl shell atomics atomics-onboard"

# ── Run ───────────────────────────────────────────────────────────────────────

# Sets $state to ok, missing or skipped, and $reason when it is not ok.
state=
run_check() {
  reason=
  skip=false
  if "check_${1//-/_}"; then
    state=ok
  elif $skip; then
    state=skipped
  else
    state=missing
  fi
}

line() {
  local text=${labels[$2]}
  [[ $1 == ok || -z $reason ]] || text+=": $reason"
  printf '%-8s %-8s %s\n' "$1" "$2" "$text"
}

if ! $check; then
  # Device code prompts go to the terminal; without one, a credential helper waits unseen.
  [[ -t 0 && -t 2 ]] || warn "no terminal: the sign-ins need one (limactl shell atomics atomics-onboard)"
  for hook in "${prepare_hooks[@]}"; do "$hook"; done
fi

missing=0
results=()
for item in "${items[@]}"; do
  run_check "$item"
  if ! $check && [[ $state == missing ]]; then
    printf '\n── %s\n' "${labels[$item]}"
    "login_${item//-/_}" || true
    run_check "$item"
  elif ! $check && [[ $state == ok ]]; then
    printf 'done before, skipped: %s\n' "${labels[$item]}"
  fi
  [[ $state != missing ]] || missing=$((missing + 1))
  results+=("$(line "$state" "$item")")
done

$check || printf '\n'
printf '%s\n' "${results[@]}"
((missing == 0))
