#!/usr/bin/env bash
# Run in the guest, with the host's ATOMICS QA queue holding the slot (see docs/nixos-vm.md).
set -euo pipefail

# Read the generated rules, rather than requiring root to list the live firewall. This also detects
# the policy on older guests, so the sub-UID regression fails on images made before the fix.
check_egress() {
  local rules status
  rules=$(systemctl cat nftables.service 2>/dev/null | sed -n 's|^ExecStart=\(/nix/store/[^ ]*-nftables-rules\)$|\1|p') || true
  if [ -z "$rules" ] || ! grep -q 'table inet atomics_egress' "$rules"; then
    printf 'egress: disabled, skipped\n'
    return
  fi
  if ! grep -q 'reject with tcp reset' "$rules"; then
    printf 'egress: logging only, skipped\n'
    return
  fi

  # Exit 7 means the TCP connection was refused. A timeout, failed namespace setup or TLS error must
  # fail this check too: none proves the policy blocked the connection. Ignore proxy environment.
  status=0
  curl --noproxy '*' --silent --show-error --insecure --output /dev/null \
    --connect-timeout 5 --max-time 10 https://1.1.1.1/ || status=$?
  if [ "$status" -ne 7 ]; then
    printf 'egress: ordinary UID was not refused (curl exit %s)\n' "$status" >&2
    return 1
  fi
  status=0
  unshare --user --map-users=auto --setuid 0 \
    curl --noproxy '*' --silent --show-error --insecure --output /dev/null \
      --connect-timeout 5 --max-time 10 https://1.1.1.1/ || status=$?
  if [ "$status" -ne 7 ]; then
    printf 'egress: subordinate UID was not refused (unshare/curl exit %s)\n' "$status" >&2
    return 1
  fi
  printf 'egress: ordinary and subordinate UIDs refused\n'
}

check_egress
# Run just the read-only network probe when diagnosing an existing guest.
if [ "${1-}" = --egress-only ]; then
  exit 0
fi

test "$(uname -m)" = aarch64
test "$(findmnt -n -o FSTYPE /)" = btrfs
grep -q 'flags: .*F' /proc/sys/fs/binfmt_misc/rosetta

for tool in node git gh claude codex cargo rustc rustup docker atomics-onboard jq ss; do
  command -v "$tool"
done

# sshd takes keys only and no root login.
sshd_config=$(sudo sshd -T)
for setting in 'passwordauthentication no' 'kbdinteractiveauthentication no' 'permitrootlogin no'; do
  grep -qix "$setting" <<<"$sshd_config"
done
printf 'sshd: keys only, no root login\n'

# ATOMICS on the Mac (programs.atomics.macDesktop): shares at the Mac's paths, its binary path
# runs this machine's atomics, and sshd replaces a stale forwarded socket.
if [ -r /etc/atomics/mac-desktop.env ]; then
  # shellcheck source=/dev/null
  . /etc/atomics/mac-desktop.env
  systemctl is-active --quiet atomics-mac-shares
  for share in $ATOMICS_MAC_SHARES; do
    path=${share%:*}
    test "$(findmnt -n -o FSTYPE -M "$path")" = virtiofs
    findmnt -n -o OPTIONS -M "$path" | tr , '\n' | grep -qx "${share##*:}"
    if [ "${share##*:}" = rw ]; then
      probe=$(mktemp "$path/.atomics-vm-check.XXXXXX")
      rm -f -- "$probe"
    fi
    printf 'share %s: virtiofs, %s\n' "$path" "${share##*:}"
  done
  for exe in $ATOMICS_MAC_EXECUTABLES; do
    test "$(readlink -f "$exe")" = "$(readlink -f "$(command -v atomics)")"
  done
  test "$(stat -c '%U %a' "${ATOMICS_MAC_SOCKET%/*}")" = "$(id -un) 700"
  grep -qix 'streamlocalbindunlink yes' <<<"$sshd_config"
  command -v atomics-vm-exec
  if [ -n "${ATOMICS_HOST_PORT-}" ]; then
    # The ATOMICS host runs here (atomics.vm.host): its own socket, its protocol server on this
    # machine's loopback alone, and an agent it starts runs here, in a Mac folder.
    systemctl is-active --quiet atomics-host
    test "$ATOMICS_SOCKET" != "$ATOMICS_MAC_SOCKET"
    test "$(stat -c '%U %a' "${ATOMICS_SOCKET%/*}")" = "$(id -un) 700"
    remote=$(atomics ctl '{"cmd":"remote"}')
    test "$(jq -r .result.protocol.url <<<"$remote")" = "ws://127.0.0.1:$ATOMICS_HOST_PORT"
    listeners=$(ss -Hltn "sport = :$ATOMICS_HOST_PORT" | awk '{print $4}')
    test "$listeners" = "127.0.0.1:$ATOMICS_HOST_PORT"
    printf 'ATOMICS host: running, atomics.v1 on 127.0.0.1:%s only\n' "$ATOMICS_HOST_PORT"
    share=
    for entry in $ATOMICS_MAC_SHARES; do
      [ "${entry##*:}" = rw ] && { share=${entry%:*}; break; }
    done
    test -n "$share"
    probe=$(mktemp -d "$share/.atomics-vm-check.XXXXXX")
    started=$(atomics ctl "$(jq -cn --arg folder "$probe" '{cmd:"start",agent:"shell",folder:$folder,isolation:"bare"}')")
    session=$(jq -r '.result.id // empty' <<<"$started")
    test -n "$session"
    pid=
    for _ in $(seq 50); do
      pid=$(atomics ctl '{"cmd":"list"}' | jq -r --arg id "$session" '.result.sessions[] | select(.id == $id) | .pid // empty')
      [ -n "$pid" ] && break
      sleep 0.2
    done
    test "$(readlink -f "/proc/$pid/cwd")" = "$(readlink -f "$probe")"
    atomics ctl "$(jq -cn --arg id "$session" '{cmd:"close",session:$id}')" >/dev/null
    rm -rf -- "$probe"
    printf 'ATOMICS host: an agent ran here (pid %s) in %s\n' "$pid" "$share"
  else
    test "$ATOMICS_SOCKET" = "$ATOMICS_MAC_SOCKET"
    if [ -S "$ATOMICS_MAC_SOCKET" ]; then
      atomics ctl '{"cmd":"list"}' >/dev/null
      printf 'forwarded socket: ATOMICS on the Mac answers\n'
    else
      printf 'forwarded socket: off (the default; docs/nixos-vm.md, The socket forward)\n'
    fi
  fi
fi

scratch=$(mktemp -d "$HOME/repos/.atomics-vm-check.XXXXXX")
cleanup() { rm -rf -- "$scratch"; }
trap cleanup EXIT
head -c 1048576 /dev/urandom > "$scratch/source"
cp --reflink=always "$scratch/source" "$scratch/clone"
cmp "$scratch/source" "$scratch/clone"
printf 'changed\n' > "$scratch/source"
test "$(wc -c < "$scratch/clone")" -eq 1048576
printf 'btrfs reflink: passed\n'

# SME, which Apple M4 and later report to guests, crashes programs here with SIGILL; the kernel hides it
# (arm64.nosme in nix/atomics-vm.nix).
if grep -qw sme /proc/cpuinfo; then
  printf 'SME is visible to the guest: boot it with arm64.nosme\n' >&2
  exit 1
fi
printf 'no SME in the guest: passed\n'

# An amd64 container runs under Rosetta (binfmt's fix-binary flag reaches into containers). Without
# a translator its shell fails with "exec format error".
test "$(docker run --rm --platform linux/amd64 docker.io/library/alpine:3.22 sh -c 'echo ran')" = ran
printf 'amd64 container under Rosetta: passed\n'

# Checks a stack adds (atomics.vm.checks), in name order.
for check in /etc/atomics/check.d/*.sh; do
  [ -f "$check" ] || continue
  printf '\n── %s\n' "${check##*/}"
  bash "$check"
done

printf 'ATOMICS VM checks passed\n'
