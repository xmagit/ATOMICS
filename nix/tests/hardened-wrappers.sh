#!/usr/bin/env bash
# Run as the VM user with an optional local image containing /bin/sh. Run again as the admin
# with --admin; the VM user cannot switch to that account. No image is pulled by this script.
set -euo pipefail
export LC_ALL=C

fail() {
  printf 'FAIL %s\n' "$*" >&2
  exit 1
}

check_wrappers() {
  python3 - "$1" <<'PY'
import errno
import os
import subprocess
import sys

route = sys.argv[1]
assert os.getuid() != 0, "run this check as the VM user"
if route == "systemd-run --user":
    with open("/proc/self/status") as status:
        assert "NoNewPrivs:\t0\n" in status.read(), "user manager imposed NoNewPrivileges"
    print("PASS user manager runs without NoNewPrivileges", flush=True)

failed = False
# Every wrapper NixOS installs, not a list: one added later must be refused too.
allowed = {"newuidmap", "newgidmap"}
wrappers = sorted(set(os.listdir("/run/wrappers/bin")) - allowed)
for name in ["su", "sudo", "sudoedit", "passwd", "mount", "unix_chkpwd"]:
    if name not in wrappers:
        print(f"FAIL {route}: expected wrapper {name} is missing", flush=True)
        failed = True
for name in wrappers:
    path = "/run/wrappers/bin/" + name
    try:
        subprocess.run([path, "--help"], stdin=subprocess.DEVNULL,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
    except OSError as error:
        if error.errno == errno.EACCES:
            print(f"PASS {route}: {name} execution refused (EACCES)", flush=True)
            continue
        detail = str(error)
    except subprocess.TimeoutExpired:
        detail = "execution was allowed (timed out)"
    else:
        detail = "execution was allowed"
    print(f"FAIL {route}: {name}: {detail}", flush=True)
    failed = True

# NixOS grants the mapping helpers either the setuid bit or file capabilities (cap_setuid/cap_setgid).
for name in ["newuidmap", "newgidmap"]:
    path = "/run/wrappers/bin/" + name
    try:
        info = os.stat(path)
        capabilities = os.getxattr(path, "security.capability") if hasattr(os, "getxattr") else b""
    except OSError:
        info, capabilities = None, b""
    privileged = info is not None and info.st_uid == 0 and (info.st_mode & 0o4000 or capabilities)
    if os.access(path, os.X_OK) and privileged:
        print(f"PASS {route}: {name} remains executable and privileged (setuid or capabilities)", flush=True)
    else:
        print(f"FAIL {route}: {name} is unavailable or has neither setuid nor file capabilities", flush=True)
        failed = True
sys.exit(1 if failed else 0)
PY
}

if [[ ${1:-} == --admin ]]; then
  [[ $# == 1 ]] || fail 'usage: hardened-wrappers.sh --admin'
  [[ $(id -u) != 0 ]] || fail 'run --admin as the admin account, not root'
  [[ $(sudo -n id -u) == 0 ]] || fail 'admin could not sudo to root'
  printf 'PASS admin sudo runs as root\n'
  exit 0
fi

# Internal mode: the user manager starts a fresh process outside the agent host's restrictions.
if [[ ${1:-} == --manager-probe ]]; then
  [[ $# == 1 ]] || fail 'unexpected probe arguments'
  check_wrappers 'systemd-run --user'
  printf 'CHECK subordinate UID/GID mappings\n'
  unshare --user --map-auto --map-root-user true || fail 'subordinate mappings failed'
  printf 'PASS subordinate UID/GID mappings work\n'
  printf 'CHECK subordinate mappings with NoNewPrivileges (must fail)\n'
  if setpriv --no-new-privs unshare --user --map-auto --map-root-user true; then
    fail 'subordinate mappings unexpectedly worked with NoNewPrivileges'
  fi
  printf 'PASS NoNewPrivileges prevents subordinate UID/GID mappings\n'
  exit 0
fi

[[ $# -le 1 && ${1:-} != --* ]] || fail 'usage: hardened-wrappers.sh [LOCAL_IMAGE] | --admin'
[[ $(id -u) != 0 ]] || fail 'run as the VM user, not root'
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
export DBUS_SESSION_BUS_ADDRESS=${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}

failed=0
check_wrappers direct || failed=1
[[ -n ${BASH_SOURCE[0]-} && -f ${BASH_SOURCE[0]} ]] || fail 'run this script from a file (it starts itself again through systemd-run --user), not from stdin'
script=$(readlink -f -- "${BASH_SOURCE[0]}")
systemd-run --user --wait --pipe bash "$script" --manager-probe || failed=1

docker_command=(docker --host "unix://$XDG_RUNTIME_DIR/docker.sock")
printf 'CHECK VM user Docker daemon\n'
security_options=$("${docker_command[@]}" info --format '{{json .SecurityOptions}}') || fail 'cannot reach the VM user Docker daemon'
[[ $security_options == *name=rootless* ]] || fail 'the VM user Docker daemon is not rootless'
printf 'PASS Docker daemon is rootless\n'
image=${1:-}
if [[ -z $image ]]; then
  images=$("${docker_command[@]}" image ls --format '{{.ID}}')
  image=${images%%$'\n'*}
fi
[[ -n $image ]] || fail 'no local image; load or pull an image containing /bin/sh, then rerun'
printf 'CHECK offline container (%s)\n' "$image"
output=$("${docker_command[@]}" run --rm --network none --pull never \
  --entrypoint /bin/sh "$image" -c 'printf hardened-wrapper-container-ok') || fail "container failed: $image"
[[ $output == hardened-wrapper-container-ok ]] || fail "container failed: $image"
printf 'PASS rootless container ran offline (%s)\n' "$image"
[[ $failed == 0 ]] || fail 'wrapper checks failed'
