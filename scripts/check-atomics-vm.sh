#!/usr/bin/env bash
# Run in the guest. `atomics vm check` supplies the admin's sshd inspection (docs/nixos-vm.md).
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

check_sshd() {
  # sshd takes keys only and no root login.
  sshd_config=${ATOMICS_CHECK_SSHD_CONFIG:-$(sudo sshd -T)}
  for setting in 'passwordauthentication no' 'kbdinteractiveauthentication no' 'permitrootlogin no'; do
    grep -qix "$setting" <<<"$sshd_config"
  done
  printf 'sshd: keys only, no root login\n'
}

# Lima's mounts as "path<TAB>rw|ro" lines: the guest path, and whether the device lets the guest write.
lima_mounts_from_mac() {
  jq -r '.[] | [(.mountPoint // .location), (if .writable then "rw" else "ro" end)] | @tsv' <<<"$1"
}

# This boot's virtiofs rows in Lima's cidata user-data, read as atomics-mac-shares reads them.
lima_mounts_from_cidata() {
  awk '
    /^[[:space:]]*- \[/ {
      line=$0; sub(/^[[:space:]]*- \[/, "", line); sub(/\][[:space:]]*$/, "", line)
      n=split(line, f, /, /)
      for (i=1; i<=n; i++) { gsub(/^"|"$/, "", f[i]) }
      if (n>=4 && f[3]=="virtiofs" && f[1]!="vz-rosetta") {
        mode="rw"; if ("," f[4] "," ~ /,ro,/) mode="ro"
        print f[2] "\t" mode
      }
    }' "$1"
}

# Each share as this system mounts it ($ATOMICS_MAC_SHARES) against Lima's list of them ($2, from $1).
compare_lima_mounts() {
  local source=$1 rows=$2 share path want got row_path row_mode failed=0
  for share in $ATOMICS_MAC_SHARES; do
    path=${share%:*} want=${share##*:} got=
    while IFS=$'\t' read -r row_path row_mode; do
      if [ "$row_path" = "$path" ]; then got=$row_mode; fi
    done <<<"$rows"
    if [ -z "$got" ]; then
      printf 'share %s: Lima (%s) does not share it\n' "$path" "$source" >&2
      failed=1
    elif [ "$got" != "$want" ] && [ "$want" = ro ]; then
      printf 'share %s: this system mounts it read-only, but Lima (%s) shares it writable, so root here could mount it read-write. On the Mac, run atomics vm update: it makes the Lima mount read-only\n' \
        "$path" "$source" >&2
      failed=1
    elif [ "$got" != "$want" ]; then
      printf 'share %s: this system mounts it writable, but Lima (%s) shares it read-only. On the Mac, run atomics vm update\n' \
        "$path" "$source" >&2
      failed=1
    fi
  done
  while IFS=$'\t' read -r row_path row_mode; do
    [ -n "$row_path" ] || continue
    case " $ATOMICS_MAC_SHARES " in *" $row_path:"*) continue ;; esac
    printf 'Lima (%s) shares %s (%s), which this system does not mount. On the Mac, run atomics vm update: it removes it\n' \
      "$source" "$row_path" "$row_mode" >&2
    failed=1
  done <<<"$rows"
  return "$failed"
}

# Lima shares each Mac folder as this system mounts it. A share the system mounts read-only must be a read-only
# device too: the guest's own mount options hold only until root mounts it again (docs/nixos-vm.md, Hardened
# mode). `atomics vm check` passes Lima's mounts from the Mac (--lima-mounts, the JSON of
# `limactl list --format '{{json .Config.Mounts}}' INSTANCE`); Lima's cidata lists this boot's, where readable.
check_lima_mounts() {
  local cidata=${ATOMICS_CHECK_CIDATA:-/mnt/lima-cidata} rows checked=
  if [ -n "${ATOMICS_CHECK_LIMA_MOUNTS-}" ]; then
    rows=$(lima_mounts_from_mac "$ATOMICS_CHECK_LIMA_MOUNTS") || {
      printf 'lima mounts: --lima-mounts is not the JSON list Lima prints\n' >&2
      return 1
    }
    compare_lima_mounts "the Mac's Lima config" "$rows" || return 1
    checked="the Mac's Lima config"
  fi
  if [ -r "$cidata/user-data" ]; then
    rows=$(lima_mounts_from_cidata "$cidata/user-data")
    compare_lima_mounts "this boot's cidata" "$rows" || return 1
    checked="${checked:+$checked and }this boot's cidata"
  fi
  if [ -z "$checked" ]; then
    if [ "${ATOMICS_CHECK_HARDENED-false}" = true ]; then
      # shellcheck disable=SC2016
      printf 'lima mounts: unknown. Run the check with atomics vm check on the Mac, or pass --lima-mounts "$(limactl list --format %s INSTANCE)"\n' \
        "'{{json .Config.Mounts}}'" >&2
      return 1
    fi
    printf 'lima mounts: not compared (atomics vm check passes them)\n'
    return 0
  fi
  printf 'lima mounts: as this system mounts them (%s)\n' "$checked"
}

# As root (--device-probe): each share this system mounts read-only is read-only at the device, so not even root
# here can write it. Root remounts it read-write and tries to create a file; whether or not the remount itself is
# refused, the file must not appear. The share is mounted read-only again afterwards, and a file that did appear
# is removed. For a throwaway VM; `atomics vm check` does not run it.
probe_read_only_devices() (
  local share path remount written failed=0 restore=()
  restore_read_only() {
    local folder
    for folder in "${restore[@]}"; do mount -o remount,ro "$folder" || true; done
  }
  trap restore_read_only EXIT
  if [ "$(id -u)" != 0 ]; then
    printf 'device probe: run it as root (sudo; the admin user in hardened mode)\n' >&2
    return 1
  fi
  # shellcheck source=/dev/null
  . "${ATOMICS_CHECK_DESKTOP_ENV:-/etc/atomics/mac-desktop.env}"
  for share in $ATOMICS_MAC_SHARES; do
    [ "${share##*:}" = ro ] || continue
    path=${share%:*}
    if ! findmnt -n -o OPTIONS -M "$path" | tr , '\n' | grep -qx ro; then
      printf 'share %s: not mounted read-only here\n' "$path" >&2
      failed=1
      continue
    fi
    written=
    if remount=$(mount -o remount,rw "$path" 2>&1); then
      restore+=("$path")
      written=$(mktemp "$path/.atomics-device-probe.XXXXXX" 2>/dev/null) || written=
      [ -z "$written" ] || rm -f -- "$written"
      mount -o remount,ro "$path"
      remount='root remounted it read-write'
    else
      remount="root's remount was refused (${remount//$'\n'/ })"
    fi
    if [ -n "$written" ]; then
      printf 'share %s: this system mounts it read-only, but root wrote a file there after a remount. The Lima mount is writable: run atomics vm update on the Mac\n' \
        "$path" >&2
      failed=1
    else
      printf 'share %s: read-only at the device: %s, and could not write it\n' "$path" "$remount"
    fi
  done
  return "$failed"
)

check_host_agent() (
  local share probe session pid started entry
  probe= session=
  cleanup_agent() {
    if [ -n "$session" ]; then
      atomics ctl "$(jq -cn --arg id "$session" '{cmd:"close",session:$id}')" >/dev/null || true
    fi
    if [ -n "$probe" ]; then rm -rf -- "$probe"; fi
  }
  trap cleanup_agent EXIT
  share=
  if [ "${ATOMICS_CHECK_HARDENED-false}" = true ]; then
    # Hardened guests develop on their own disk. Their one writable share, the exchange, holds files only.
    share="$HOME/repos"
    mkdir -p -- "$share"
  else
    for entry in $ATOMICS_MAC_SHARES; do
      [ "${entry##*:}" = rw ] && { share=${entry%:*}; break; }
    done
  fi
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
  session=
  printf 'ATOMICS host: an agent ran here (pid %s) in %s\n' "$pid" "$share"
)

check_arguments() {
  ATOMICS_CHECK_EGRESS_ONLY=false
  ATOMICS_CHECK_HARDENED=false
  ATOMICS_CHECK_LIMA_MOUNTS=
  ATOMICS_CHECK_DEVICE_PROBE=false
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --sshd-config) ATOMICS_CHECK_SSHD_CONFIG=$2; shift 2 ;;
      --hardened) ATOMICS_CHECK_HARDENED=$2; shift 2 ;;
      --lima-mounts) ATOMICS_CHECK_LIMA_MOUNTS=$2; shift 2 ;;
      --egress-only) ATOMICS_CHECK_EGRESS_ONLY=true; shift ;;
      --device-probe) ATOMICS_CHECK_DEVICE_PROBE=true; shift ;;
      *) printf 'Unknown check argument: %s\n' "$1" >&2; return 1 ;;
    esac
  done
}

main() {
  check_arguments "$@"
  if [ "$ATOMICS_CHECK_DEVICE_PROBE" = true ]; then
    probe_read_only_devices
    return
  fi
  check_egress
  [ "$ATOMICS_CHECK_EGRESS_ONLY" = false ] || return 0

  test "$(uname -m)" = aarch64
  test "$(findmnt -n -o FSTYPE /)" = btrfs
  grep -q 'flags: .*F' /proc/sys/fs/binfmt_misc/rosetta

  for tool in node git gh claude codex cargo rustc rustup gcc pkg-config python3 mktemp docker atomics-onboard jq ss; do
    command -v "$tool"
  done

  check_sshd

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
    check_lima_mounts
    for exe in $ATOMICS_MAC_EXECUTABLES; do
      test "$(readlink -f "$exe")" = "$(readlink -f "$(command -v atomics)")"
    done
    test "$(stat -c '%U %a' "${ATOMICS_MAC_SOCKET%/*}")" = "$(id -un) 700"
    grep -qix 'streamlocalbindunlink yes' <<<"$sshd_config"
    command -v atomics-vm-exec
    if [ -n "${ATOMICS_HOST_PORT-}" ]; then
      # The ATOMICS host runs here (atomics.vm.host): its own socket, its protocol server on this
      # machine's loopback alone, and an agent it starts runs here.
      systemctl is-active --quiet atomics-host
      test "$ATOMICS_SOCKET" != "$ATOMICS_MAC_SOCKET"
      test "$(stat -c '%U %a' "${ATOMICS_SOCKET%/*}")" = "$(id -un) 700"
      remote=$(atomics ctl '{"cmd":"remote"}')
      test "$(jq -r .result.protocol.url <<<"$remote")" = "ws://127.0.0.1:$ATOMICS_HOST_PORT"
      listeners=$(ss -Hltn "sport = :$ATOMICS_HOST_PORT" | awk '{print $4}')
      test "$listeners" = "127.0.0.1:$ATOMICS_HOST_PORT"
      printf 'ATOMICS host: running, atomics.v1 on 127.0.0.1:%s only\n' "$ATOMICS_HOST_PORT"
      check_host_agent
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
}

if [ "${BASH_SOURCE[0]-}" = "$0" ] || [ -z "${BASH_SOURCE[0]-}" ]; then
  main "$@"
fi
