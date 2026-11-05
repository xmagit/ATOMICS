#!/usr/bin/env bash
# Tests atomics-onboard's built-in steps and its step files against stub tools, in a throwaway HOME:
#
#   onboard-test.sh ATOMICS_ONBOARD [--journal] [DIR...]
#
# It checks that the first run signs in to what is missing, that a second run skips every finished
# step, that --check reports every item without values, that a step file adds an item, an option and
# a preparation, that credential files stay private, and that the canary is found only in the
# tools' own credential files: not in atomics-onboard's output or Git's config, nor in DIR... (such as
# /nix/store or this checkout) or, with --journal, the systemd journal. The canary is never printed.
# onboard-test-lib.sh is the harness.
set -euo pipefail

# shellcheck source=nix/vm/onboard-test-lib.sh
source "${ONBOARD_TEST_LIB:-$(dirname -- "${BASH_SOURCE[0]}")/onboard-test-lib.sh}" "$@"

# ── Stub tools ───────────────────────────────────────────────────────────────

stub claude <<'EOF'
creds=$HOME/.claude/.credentials.json
case "${1:-} ${2:-}" in
  "auth status")
    if [[ -f $creds ]]; then
      printf '{"loggedIn":true,"authMethod":"%s","email":"%s@example.com"}\n' "${STUB_CLAUDE_METHOD:-claude.ai}" "$STUB_CANARY"
      exit 0
    fi
    printf '{"loggedIn":false,"authMethod":"none"}\n'
    exit 1
    ;;
  "auth login")
    printf 'Open https://claude.ai/oauth/authorize in a browser, then paste the code: '
    IFS= read -r code
    mkdir -p "${creds%/*}"
    printf '{"claudeAiOauth":{"accessToken":"%s"}}\n' "$code" >"$creds"
    printf '\nLogin successful.\n'
    ;;
  *) exit 3 ;;
esac
EOF

stub codex <<'EOF'
auth=$HOME/.codex/auth.json
case "${1:-} ${2:-}" in
  "login status")
    if [[ -f $auth ]]; then
      printf 'Logged in using an API key - %s\n' "$STUB_CANARY"
      exit 0
    fi
    printf 'Not logged in\n'
    exit 1
    ;;
  "login --device-auth")
    printf 'Open https://auth.openai.com/codex/device and enter code ABCD-1234\n'
    IFS= read -r token
    mkdir -p "${auth%/*}"
    printf '{"tokens":{"refresh_token":"%s"}}\n' "$token" >"$auth"
    ;;
  *) exit 3 ;;
esac
EOF

# The GitHub CLI without a keyring keeps its token in hosts.yml, as here.
stub gh <<'EOF'
hosts=$HOME/.config/gh/hosts.yml
case "${1:-} ${2:-}" in
  "auth status")
    [[ " $* " == *" --hostname github.com "* ]] || exit 3
    if [[ -f $hosts ]]; then
      printf 'github.com\n  Logged in to github.com account octo\n  - Token: %s\n' "$STUB_CANARY"
      exit 0
    fi
    printf 'You are not logged into any GitHub hosts. To log in, run: gh auth login\n' >&2
    exit 1
    ;;
  "auth login")
    [[ " $* " == *" --hostname github.com "* && " $* " == *" --web "* ]] || exit 3
    printf 'First copy your one-time code: ABCD-1234\nOpen https://github.com/login/device in your web browser\n' >&2
    IFS= read -r token
    mkdir -p "${hosts%/*}"
    printf 'github.com:\n    oauth_token: %s\n    git_protocol: https\n' "$token" >"$hosts"
    ;;
  "auth setup-git")
    git config --global --replace-all credential.https://github.com.helper ''
    # nixpkgs' gh writes the path of the program behind its wrapper, in the Nix store.
    git config --global --add credential.https://github.com.helper "!${STUB_GH_HELPER:-$0} auth git-credential"
    ;;
  *) exit 3 ;;
esac
EOF

logins() { grep -cE '^(claude auth login|codex login --device-auth|gh auth login)' "$logs/$1.calls" || true; }
helpers() { git config --global --get-all credential.https://github.com.helper | sed 's#^!.*/gh #!gh #' | tr '\n' ,; }

# A step file as a stack would add one: an item, an option and a preparation.
mkdir -p "$work/steps" "$work/no-steps" "$work/bad-steps" "$work/abs-private"
cat >"$work/steps/50-demo.sh" <<'EOF'
# shellcheck shell=bash
demo_name=
demo_take_name() { [[ $1 =~ ^[a-z]+$ ]] || die "--demo-name: lower-case letters only"; demo_name=$1; }
demo_prepare() { [[ -z $demo_name ]] || printf '%s\n' "$demo_name" >"$HOME/demo-name"; }
check_demo() { [[ -f $HOME/demo-done ]] || { reason="not done"; return 1; }; }
login_demo() { touch "$HOME/demo-done"; }
onboard_option demo-name "a name for the demo step" demo_take_name
onboard_prepare demo_prepare
onboard_step demo "Demo step from a step file"
onboard_private demo-done
EOF
printf 'not a step file, not sourced\n' >"$work/steps/README"
printf 'onboard_step Bad "an invalid name"\n' >"$work/bad-steps/10-bad.sh"
printf 'onboard_private /etc/shadow\n' >"$work/abs-private/10-abs.sh"

# ── 1. --check on a fresh VM without step files: the built-in items only ─────

fresh_home vm
export ATOMICS_ONBOARD_DIR=$work/no-steps
items="claude codex gh private host"
input=
run check-builtin --check
expect "fresh --check fails" test "$status" -eq 1
expect "fresh --check lists the built-in items" test "$(all_states check-builtin)" \
  = "claude=missing codex=missing gh=missing private=ok host=skipped "
expect "fresh --check lists exactly five lines" test "$(wc -l <"$logs/check-builtin")" -eq 5
expect "fresh --check signs in to nothing" test "$(logins check-builtin)" -eq 0
expect "fresh --check leaves Git's config alone" test ! -e "$HOME/.gitconfig"

# ── 2. A step file adds its item before the host check ───────────────────────

export ATOMICS_ONBOARD_DIR=$work/steps
items="claude codex gh demo private host"
run check-fresh --check
expect "a step file's item is listed" test "$(all_states check-fresh)" \
  = "claude=missing codex=missing gh=missing demo=missing private=ok host=skipped "
expect "the host check stays last" test "$(tail -n 1 "$logs/check-fresh" | awk '{print $2}')" = host
run help --help
expect "--help lists a step file's option" grep -q -- '--demo-name VALUE' "$logs/help"
ATOMICS_ONBOARD_DIR=$work/bad-steps run bad --check
expect "a step file with an invalid name is refused" test "$status" -eq 2

# ── 3. First run: every sign-in once ──────────────────────────────────────────

# What the person types at the Claude, Codex and GitHub prompts.
printf -v input '%s\n' "$canary" "$canary" "$canary"
run first --demo-name alpha
expect "first run succeeds (host skipped without a socket)" test "$status" -eq 0
expect "first run ends with every item ok" test "$(all_states first)" \
  = "claude=ok codex=ok gh=ok demo=ok private=ok host=skipped "
expect "first run signs in to Claude" called first "claude auth login"
expect "first run signs in to Codex with a device code" called first "codex login --device-auth"
expect "first run signs in to GitHub in the browser" called first \
  "gh auth login --hostname github.com --git-protocol https --web"
expect "first run makes gh Git's helper for github.com" called first "gh auth setup-git --hostname github.com"
expect "Git asks only gh for github.com" test "$(helpers)" = ",!gh auth git-credential,"
expect "a step file's option reaches its preparation" test "$(cat "$HOME/demo-name")" = alpha
expect "a step file's sign-in ran" test -e "$HOME/demo-done"
expect "sign-ins create private credential files" test \
  "$(stat -c %a "$HOME/.codex/auth.json" "$HOME/.codex" "$HOME/.config/gh/hosts.yml" "$HOME/.config/gh" | tr '\n' ' ')" \
  = "600 700 600 700 "
expect "a step file's credential file is private" test "$(stat -c %a "$HOME/demo-done")" = 600

# ── 4. Second run: nothing to do ──────────────────────────────────────────────

input=
run second
expect "second run succeeds" test "$status" -eq 0
expect "second run signs in to nothing" test "$(logins second)" -eq 0
expect "second run skips all five finished steps" test "$(grep -c '^done before, skipped: ' "$logs/second")" -eq 5
expect "second run leaves Git's helpers as they were" test "$(helpers)" = ",!gh auth git-credential,"

run check-done --check
expect "--check after onboarding passes" test "$status" -eq 0
expect "--check after onboarding lists every item" test "$(all_states check-done)" \
  = "claude=ok codex=ok gh=ok demo=ok private=ok host=skipped "

# ── 5. The forwarded socket, gh without Git, a non-subscription login, options ──

STUB_HOST=up run check-host-up --check
expect "a host that answers is ok" test "$(state_of check-host-up host)" = ok
STUB_HOST=down run check-host-down --check
expect "a forwarded socket that does not answer is missing" test "$(state_of check-host-down host)" = missing
expect "a missing item fails --check" test "$status" -eq 1

git config --global --unset-all credential.https://github.com.helper
run check-no-helper --check
expect "gh without Git's helper is missing" test "$(state_of check-no-helper gh)" = missing
run fix-helper
expect "a signed-in gh is only made Git's helper again" test "$(logins fix-helper)" -eq 0
expect "a signed-in gh becomes Git's helper" test "$(helpers)" = ",!gh auth git-credential,"

run check-store-helper --check
expect "a helper on PATH is ok" test "$(state_of check-store-helper gh)" = ok
git config --global --replace-all credential.https://github.com.helper \
  '!/nix/store/00000000000000000000000000000000-gh/bin/.gh-wrapped auth git-credential' '^!'
run check-store-helper --check
expect "a helper in the Nix store is missing" test "$(state_of check-store-helper gh)" = missing
STUB_GH_HELPER=/nix/store/00000000000000000000000000000000-gh/bin/.gh-wrapped run fix-store-helper
expect "a store helper signs in to nothing" test "$(logins fix-store-helper)" -eq 0
expect "a store helper becomes gh on PATH" test "$(helpers)" = ",!gh auth git-credential,"
expect "no helper names the Nix store" test "$(git config --global --get-all credential.https://github.com.helper | grep -c /nix/store)" -eq 0

# A credential file or folder others can open: --check names the first, a run fixes the modes.
chmod 644 "$HOME/.codex/auth.json"
chmod 755 "$HOME/.config/gh"
run check-open --check
expect "an open credential file is missing" test "$(state_of check-open private)" = missing
expect "--check names the open file, not a value" grep -qx \
  'missing  private  Credential files private to this user (no group or other access): .codex/auth.json in the home folder is open to other users (and 1 more)' \
  "$logs/check-open"
expect "--check changes no mode" test "$(stat -c %a "$HOME/.codex/auth.json")" = 644
run fix-open
expect "a run makes credential files private again" test "$(state_of fix-open private)" = ok
expect "the file and the folder are private again" test \
  "$(stat -c %a "$HOME/.codex/auth.json" "$HOME/.config/gh" | tr '\n' ' ')" = "600 700 "
expect "fixing modes signs in to nothing" test "$(logins fix-open)" -eq 0
ATOMICS_ONBOARD_DIR=$work/abs-private run abs-private --check
expect "a step file's private path must be relative" test "$status" -eq 2

STUB_CLAUDE_METHOD=api_key run check-api-key --check
expect "Claude with an API key is not a subscription" test "$(state_of check-api-key claude)" = missing

run check-with-option --check --demo-name beta
expect "--check refuses to change settings" test "$status" -eq 2
run bad-value --demo-name=Beta1
expect "a step file's handler refuses a bad value" test "$status" -eq 2
run unknown --no-such-option x
expect "an unknown option is refused" test "$status" -eq 2

# ── 6. Not in the VM ──────────────────────────────────────────────────────────

# Bash keeps an OSTYPE it inherits.
OSTYPE=darwin25.1.0 run mac --check
expect "on the Mac it refuses to run" test "$status" -eq 2
expect "on the Mac it says where to run it" grep -q 'inside the ATOMICS VM' "$logs/mac"

# ── 7. Where the canary is, and where it is not ───────────────────────────────

show_runs="first check-host-down"
finish '\.claude/\.credentials\.json|\.codex/auth\.json|\.config/gh/hosts\.yml' 3
