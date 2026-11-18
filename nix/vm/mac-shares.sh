#!/usr/bin/env bash
# Boot-time Mac identity and shares. The first argument is a trusted Nix-generated configuration;
# cidata is data, never sourced as shell code. Lima's param.env uses literal KEY=value lines.
#
# This runs as root in a tree that mirrors the Mac's paths, where the VM's user, and so every agent,
# also runs. Root never creates, changes, links or mounts at a path whose components that user could
# rename or replace: every folder from / down to a mount point or link is root's and writable by root
# alone, and the walk below makes it so top-down, each step inside a parent that is already root's.
# The one folder the VM's user owns is the socket's, a leaf that root never looks into.
# A link or a non-folder on any of these paths fails the unit before the link or non-folder is used.
set -euo pipefail
. "$1"
# Tests run the script unprivileged against a scratch tree; the VM uses the real root.
fsroot=${fsroot-}
root_user=${root_user-root}
root_group=${root_group-root}
noexec=${noexec-false}

fail() { printf 'atomics-mac-shares: %s\n' "$*" >&2; exit 1; }
valid_path() {
  [[ $1 =~ ^/[A-Za-z0-9._@+/-]+$ && $1 != */ && $1 != *'/../'* && $1 != */.. && $1 != *'/./'* && $1 != */. && $1 != *//* ]]
}
within() { [ "$2" = "$1" ] || [[ $2 == "$1"/* ]]; }

if [ "$dynamic" = true ]; then
  mac_user=$(sed -n 's/^PARAM_ATOMICS_MAC_USER=//p' "$cidata/param.env")
  [[ $mac_user =~ ^[A-Za-z0-9_][A-Za-z0-9._-]*$ && $mac_user != *..* ]] || {
    echo 'Missing or invalid ATOMICS_MAC_USER in Lima cidata' >&2; exit 1;
  }
  mac_home=${mac_home//@ATOMICS_MAC_USER@/$mac_user}
  socket=${socket//@ATOMICS_MAC_USER@/$mac_user}
  forward=${forward//@ATOMICS_MAC_USER@/$mac_user}
  executables=${executables//@ATOMICS_MAC_USER@/$mac_user}
fi
# These are the flow-style mount rows emitted by Lima, not arbitrary cloud-init instructions.
# Read only virtiofs rows; Rosetta is mounted by NixOS's own service.
mounts=$(awk '
  /^[[:space:]]*- \[/ {
    line=$0; sub(/^[[:space:]]*- \[/, "", line); sub(/\][[:space:]]*$/, "", line)
    n=split(line, f, /, /)
    for (i=1; i<=n; i++) { gsub(/^"|"$/, "", f[i]) }
    if (n>=4 && f[3]=="virtiofs" && f[1]!="vz-rosetta") {
      mode="rw"; if ("," f[4] "," ~ /,ro,/) mode="ro"
      print f[1] " " f[2] " " mode
    }
  }' "$cidata/user-data")
if [ "$dynamic" = true ]; then
  shares=
  while read -r tag path mode; do
    [ -n "$tag" ] || continue
    valid_path "$path" || { echo 'Invalid cidata mount path' >&2; exit 1; }
    case $path in "$mac_home"/*|/Volumes/*) ;; *) echo 'Unexpected Mac share path' >&2; exit 1;; esac
    shares="$shares $path:$mode"
  done <<< "$mounts"
  [ -n "$shares" ] || { echo 'Lima supplies no Mac shares' >&2; exit 1; }
fi

# Validate all inputs before changing anything. Never place executable links inside a Mac share.
for path in "$mac_home" "$socket" "$forward" $executables; do
  valid_path "$path" || { echo 'Invalid Mac path' >&2; exit 1; }
done
[[ $mac_home =~ ^/Users/[^/]+$ ]] || fail "the Mac home $mac_home is not /Users/<name>"
[ "${forward%/*}" != "${socket%/*}" ] && [ "${forward%/*/*}" = "${socket%/*}" ] ||
  fail "the socket's own folder must sit beside the socket"
for share in $shares; do
  path=${share%:*}
  for exe in "$socket" "${forward%/*}" $executables; do
    case $exe in "$path"|"$path"/*) echo 'Executable or socket is inside a share' >&2; exit 1;; esac
  done
  # The VM's user owns the socket's own folder: root never works inside it.
  ! within "${forward%/*}" "$path" || fail "share $path is inside the socket's folder ${forward%/*}"
  ! within "$path" "${forward%/*}" || fail "the socket's folder ${forward%/*} is inside share $path"
done
for exe in $executables; do
  ! within "${forward%/*}" "$exe" || fail "$exe is inside the socket's folder ${forward%/*}"
done
for share in $shares; do
  path=${share%:*}
  for other in $shares; do
    other=${other%:*}
    [ "$other" = "$path" ] || ! within "$other" "$path" || fail "share $path is inside share $other"
  done
done
# Everything root changes lies in the trees that mirror the Mac. Elsewhere it changes nothing.
managed() {
  case $1 in /Users|/Users/*|/Applications|/Applications/*|/Volumes|/Volumes/*) return 0;; esac
  return 1
}
for path in "$socket" $executables; do
  managed "$path" || fail "$path is outside /Users, /Applications and /Volumes"
done
for share in $shares; do
  managed "${share%:*}" || fail "${share%:*} is outside /Users, /Applications and /Volumes"
done

# One folder on a path. `check` only looks; `apply` makes it what it must be. A folder still owned by
# the VM's user from an earlier layout is taken over: root takes it first, so its old owner can no
# longer change its mode; then its mode drops the group and others' write bits, and the result is
# checked. Its parent is already root's, so between the look and the change nobody but root can
# replace the entry. Whatever was created in it meanwhile is looked at by the next step, once it is
# root's alone.
own() {
  local path=$1 real=$2 user=$3 group=$4 mode=$5 want
  chown -h "$user:$group" -- "$real"
  chmod "0$mode" -- "$real"
  # The VM user's own folder (the socket's) is its to change from now on; root's must stay as set.
  want="$user $group"
  [ "$user" != "$root_user" ] || want="$want ${mode#0}"
  [ "$(stat -c "%U %G$([ "$user" != "$root_user" ] || echo ' %a')" -- "$real")" = "$want" ] ||
    fail "$path did not become $user:$group ${mode#0}; refusing to go on"
}
step() {
  local how=$1 path=$2 user=$3 group=$4 mode=$5 real=$fsroot$2 owner perms
  if [ -L "$real" ]; then
    fail "$path is a symbolic link; refusing to follow it (nothing was changed under it)"
  fi
  if [ ! -e "$real" ]; then
    if [ "$how" = check ]; then tree_ready=false; return 0; fi
    mkdir -m "$mode" -- "$real"
    own "$path" "$real" "$user" "$group" "$mode"
    return 0
  fi
  [ -d "$real" ] || fail "$path is not a folder; refusing to use it"
  owner=$(stat -c %U -- "$real")
  perms=$(stat -c %a -- "$real")
  if [ "$owner" = "$user" ] && [ "$(stat -c %G -- "$real")" = "$group" ] && [ "$perms" = "${mode#0}" ]; then
    return 0
  fi
  managed "$path" || fail "$path belongs to $owner with mode $perms; refusing to change it"
  if [ "$how" = check ]; then tree_ready=false; return 0; fi
  own "$path" "$real" "$user" "$group" "$mode"
}
# Every folder from / down to `$2`, each root's (the last one `$3:$4 $5` when given).
tree() {
  local how=$1 path=$2 sofar='' part
  local -a parts
  tree_ready=true
  IFS=/ read -ra parts <<< "${path#/}"
  for part in "${parts[@]}"; do
    sofar=$sofar/$part
    if [ "$sofar" = "$path" ] && [ $# -gt 2 ]; then
      step "$how" "$sofar" "$3" "$4" "$5"
    else
      step "$how" "$sofar" "$root_user" "$root_group" 0755
    fi
    [ "$tree_ready" = true ] || break
  done
}
# A link root keeps: absent, or a link it may replace; anything else is refused.
link_target() {
  local real=$fsroot$1
  if [ ! -L "$real" ] && [ -e "$real" ]; then
    [ -S "$real" ] || fail "$1 exists and is neither a link nor a stale socket; refusing to replace it"
  fi
}
layout() {
  local how=$1 exe share path tree_ready
  tree "$how" "${socket%/*}"
  [ "$tree_ready" = false ] || link_target "$socket"
  # The VM's user binds the forwarded socket here (sshd); root never looks inside.
  tree "$how" "${forward%/*}" "$owner" "$group" 0700
  for exe in $executables; do
    tree "$how" "${exe%/*}"
    [ "$tree_ready" = false ] || link_target "$exe"
  done
  for share in $shares; do
    path=${share%:*}
    tree "$how" "${path%/*}"
    [ "$tree_ready" = true ] || continue
    if [ -L "$fsroot$path" ]; then
      fail "$path is a symbolic link; refusing to mount on it"
    fi
    if ! mountpoint -q "$fsroot$path"; then
      step "$how" "$path" "$root_user" "$root_group" 0755
    fi
  done
}

[[ $owner =~ ^[a-z_][a-z0-9_-]*$ && $group =~ ^[a-z_][a-z0-9_-]*$ ]] || fail "bad VM user $owner or group $group"
[ "$owner" != "$root_user" ] || [ -n "$fsroot" ] || fail "the VM user must not be root"

# Preflight stops each walk at the first missing or nonconforming folder.
# Apply secures that folder before examining any of its children.
layout check
layout apply

# Every name below is in a root-owned folder now: no link swap can redirect these.
forward_folder=${forward%/*}
socket_link=${forward_folder##*/}/${forward##*/}
ln -sfnT -- "$socket_link" "$fsroot$socket"
[ "$(readlink -- "$fsroot$socket")" = "$socket_link" ] || fail "could not link $socket"
for exe in $executables; do
  ln -sfnT -- "$binary" "$fsroot$exe"
done
options=nosuid,nodev
[ "$noexec" = true ] && options=$options,noexec
host_folder=$guest_home
attachment_exchange=
for share in $shares; do
  path=${share%:*} mode=${share##*:}
  tag=$(awk -v want="$path" '$2==want {print $1}' <<< "$mounts")
  [[ $tag =~ ^[A-Za-z0-9._-]+$ ]] || { echo "Lima does not share $path" >&2; exit 1; }
  if ! mountpoint -q "$fsroot$path"; then
    mount -t virtiofs -o "$mode,$options" "$tag" "$fsroot$path"
  else
    # Mounted before this run (a switch without a reboot): bring its options in line, noexec too.
    mount -o "remount,$mode,$options" "$fsroot$path"
  fi
  # Agents start in the first writable share, unless shares run nothing (hardened mode): then the
  # writable one is the exchange, and agents work on the VM's own disk.
  if [ "$mode" = rw ] && [ "$noexec" != true ] && [ "$host_folder" = "$guest_home" ]; then host_folder=$path; fi
  if [ "$mode" = rw ] && [ "$path" = "$mac_home/atomics-exchange" ]; then attachment_exchange=$path; fi
done
mkdir -p "${output%/*}"
# All paths above use the restricted alphabet: safe for both shell sourcing and word lists.
printf 'ATOMICS_MAC_SOCKET=%s\nATOMICS_MAC_SOCKET_FOLDER=%s\nATOMICS_MAC_SHARES="%s"\nATOMICS_MAC_EXECUTABLES="%s"\nATOMICS_HOST_PORT=%s\nATOMICS_HOST_FOLDER=%s\n' \
  "$socket" "${forward%/*}" "$shares" "$executables" "$host_port" "$host_folder" > "$output"
printf 'ATOMICS_EXCHANGE=%s\n' "$attachment_exchange" >> "$output"
