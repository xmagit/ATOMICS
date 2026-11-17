#!/usr/bin/env bash
# Exercises the Mac wrappers of programs.atomics.macDesktop against the guest helper, with an
# ssh stand-in that runs the remote command in a clean environment as sshd would.
# Usage: mac-desktop.sh BUNDLE VM_EXEC_PACKAGE
set -euo pipefail

bundle=$1
vm_exec=$2/bin/atomics-vm-exec
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# The Mac has no Nix: nothing in the bundle may point into the store.
if grep -rq /nix/store "$bundle"; then
  grep -rn /nix/store "$bundle" >&2
  fail "the Mac bundle references /nix/store"
fi

mkdir -p "$work/home/.lima/atomics" "$work/bin" "$work/vm-bin" "$work/folder with space"
touch "$work/home/.lima/atomics/ssh.config"
log=$work/ssh.log
cat >"$work/bin/ssh" <<EOF
#!/bin/sh
# ssh stand-in: records the call and runs the remote command here, in sshd's clean environment.
printf '%s\n' "\$*" >>"$log"
forward=
while [ "\$#" -gt 0 ]; do
  case \$1 in
    -F | -o | -e | -R) [ "\$1" = -R ] && forward=1; shift 2 ;;
    -tt | -T | -N) shift ;;
    lima-atomics) shift; break ;;
    *) echo "ssh stand-in: unexpected \$1" >&2; exit 2 ;;
  esac
done
[ -z "\$forward" ] && [ -z "\${SSH_DRY_RUN-}" ] || exit 0
command=\$(printf '%s' "\$*" | sed "s|/run/current-system/sw/bin/atomics-vm-exec|$vm_exec|")
exec env -i HOME="$work/vm-home" PATH="$work/vm-bin:$PATH" SHELL=/bin/sh XDG_RUNTIME_DIR="$work/runtime" \
  bash -c "\$command"
EOF
chmod +x "$work/bin/ssh"
mkdir -p "$work/vm-home" "$work/runtime"
# Programs "in the VM": argument printers named like the wrapped agents.
for program in claude codex; do
  cat >"$work/vm-bin/$program" <<EOF
#!/bin/sh
printf '%s:' $program
printf '<%s>' "\$@"
printf '\n'
EOF
  chmod +x "$work/vm-bin/$program"
done
export HOME=$work/home PATH=$work/bin:$PATH
unset LIMA_HOME

run() { (cd "$work/folder with space" && "$@" </dev/null); }

# Arguments arrive exactly: quotes, globs, variables, newlines, empty strings.
# shellcheck disable=SC2016
args=('a b' "it's" '"q"' '*' '$HOME' $'two\nlines' '' 'ünï' '--')
expected=$(printf '<%s>' "${args[@]}")
[[ $(run "$bundle/bin/claude" "${args[@]}") == "claude:$expected" ]] || fail "claude arguments"
[[ $(run "$bundle/bin/codex" --version) == "codex:<--version>" ]] || fail "codex --version"
[[ $(run "$bundle/bin/atomics-vm-run" printf '<%s>' "${args[@]}") == "$expected" ]] || fail "run arguments"

# The program starts in the same folder.
[[ $(run "$bundle/bin/atomics-vm-run" pwd) == "$work/folder with space" ]] || fail "folder"

# Only the listed variables cross; the Mac's PATH and credentials do not.
environment=$(cd "$work/folder with space" && TERM=xterm-test ATOMICS_HOOK_PANE=pane-1 \
  CLAUDE_CODE_OAUTH_TOKEN=secret CARGO_MAKEFLAGS=-j "$bundle/bin/atomics-vm-run" env </dev/null)
grep -qx 'TERM=xterm-test' <<<"$environment" || fail "TERM was not forwarded"
grep -qx 'ATOMICS_HOOK_PANE=pane-1' <<<"$environment" || fail "the hook pane was not forwarded"
! grep -q 'CLAUDE_CODE_OAUTH_TOKEN\|CARGO_MAKEFLAGS' <<<"$environment" || fail "a Mac variable crossed"

# Agent sessions use a private connection without keystroke obfuscation or an escape character.
: >"$log"
run "$bundle/bin/claude" >/dev/null
for option in '-e none' 'ObscureKeystrokeTiming=no' 'ControlMaster=no' 'ControlPath=none' '-T '; do
  grep -qF -- "$option" "$log" || fail "agent ssh lacks $option: $(cat "$log")"
done

# Requests beyond one sshd command line are staged on stdin first. (Linux caps one argument at
# 128 KiB, so three make up the size here; macOS has no such cap.)
part=$(head -c 50000 /dev/zero | tr '\0' x)
: >"$log"
[[ $(run "$bundle/bin/atomics-vm-run" printf '%s' "$part" "$part" "$part" | wc -c) -eq 150000 ]] ||
  fail "a staged request"
{ grep -q -- '--stage$' "$log" && grep -q -- '--staged payload\.' "$log"; } || fail "large request was not staged"

# A folder the VM does not have is refused with the shares it does have.
set +e
message=$(cd / && "$vm_exec" "$(printf '%s\0' 1 "$work/missing" -- true | base64 -w0)" 2>&1)
status=$?
set -e
[[ $status -eq 66 && $message == *"is not in this VM"*"/repos"* ]] || fail "missing folder: $status $message"
[[ $("$vm_exec" "$(printf 'garbage' | base64 -w0)" 2>&1 || true) == *"unreadable request"* ]] || fail "bad payload"

# The forward makes each socket's folder, then forwards every socket at its own path.
: >"$log"
run "$bundle/bin/atomics-vm-forward" "$work/sockets/a.sock" "$work/other/b.sock"
[[ -d $work/sockets && -d $work/other ]] || fail "socket folders"
{ grep -qF -- "-N -T" "$log" && grep -qF -- "ExitOnForwardFailure=yes" "$log"; } || fail "forward options: $(cat "$log")"
grep -qF -- "-R $work/sockets/a.sock:$work/sockets/a.sock -R $work/other/b.sock:$work/other/b.sock" "$log" ||
  fail "forwards: $(cat "$log")"
: >"$log"
SSH_DRY_RUN=1 run "$bundle/bin/atomics-vm-forward"
# The default socket binds in the VM user's own folder, which the socket's Mac path there links to.
grep -qE -- '-R (/[^ :]+)/forward/(atomics\.sock):\1/\2 lima-atomics$' "$log" || fail "default socket forward: $(cat "$log")"
! run "$bundle/bin/atomics-vm-forward" relative.sock 2>/dev/null || fail "relative socket accepted"

# Without the instance there is a clear error, not an ssh failure.
rm "$work/home/.lima/atomics/ssh.config"
set +e
message=$(run "$bundle/bin/claude" 2>&1)
status=$?
set -e
[[ $status -eq 69 && $message == *"no Lima instance atomics"* ]] || fail "missing instance: $status $message"

# The Lima definition mounts the shares at their own paths and is not plain (plain drops mounts).
remarshal --if yaml --of json "$bundle/lima.yaml" >"$work/lima.json"
jq -e '.plain == false and .mountType == "virtiofs" and .vmType == "vz"
  and ([.mounts[] | select(.location == .mountPoint)] | length) == (.mounts | length)
  and any(.mounts[]; .writable == true and (.location | endswith("/repos")))
  and .user.uid != null and .vmOpts.vz.rosetta.enabled' "$work/lima.json" >/dev/null || fail "lima.yaml: $(cat "$work/lima.json")"

# The updater can reproduce the image's build inputs from the installed bundle alone.
jq -e --slurpfile lima "$work/lima.json" '
  .version == 1 and .build.cpus == $lima[0].cpus
  and ((.build.memoryGiB | tostring) + "GiB") == $lima[0].memory
  and ((.build.diskGiB | tostring) + "GiB") == $lima[0].disk
' "$bundle/atomics-vm.json" >/dev/null || fail "atomics-vm.json has different build settings from lima.yaml"

# The config snippet parses, runs every agent bare through the bundle, and the chief too.
remarshal --if toml --of json "$bundle/config.d.toml" >"$work/config.json"
jq -e '([.agents[] | select(.isolation == "bare")] | length) == (.agents | length)
  and any(.agents[]; .name == "claude" and .kind == "claude" and (.command[0] | endswith("/bin/claude")))
  and any(.agents[]; .name == "codex" and .kind == "generic" and (.command[0] | endswith("/bin/codex")))
  and any(.agents[]; .name == "shell" and .kind == "shell")
  and (.chief.command[0] | endswith("/bin/claude"))
  and .isolation.bare_relay == false' "$work/config.json" >/dev/null || fail "config.d.toml: $(cat "$work/config.json")"

printf 'atomics-vm-mac: all checks passed\n'
